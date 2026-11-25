//! RBAC scope with **monotone narrowing**.
//!
//! A [`Scope`] is a role plus the set of sites and (optionally) the set of machines a
//! caller may see. [`Scope::narrow`] is the only way to derive a new scope from an existing
//! one, and it can only *intersect*: every requested site or machine must already be
//! permitted, otherwise the request is rejected outright (it is never silently clipped and
//! never widens). Therefore, for every narrowing `child = parent.narrow(req)`:
//!
//! ```text
//! child.permits(site, machine)  ==>  parent.permits(site, machine)
//! ```

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::catalog::Catalog;

/// Role of the principal. Every tool is read-only, so the role is carried for
/// attribution and audit; visibility is controlled by the site/machine sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    /// Read-only viewer.
    Viewer,
    /// Line operator (still read-only through this server).
    Operator,
}

/// What a caller may see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Scope {
    role: Role,
    sites: BTreeSet<String>,
    /// `None` = every machine on the permitted sites.
    machines: Option<BTreeSet<String>>,
}

/// A narrowing request, as passed in the optional `scope` tool argument.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScopeRequest {
    /// Restrict to these sites (each must already be permitted).
    #[serde(default)]
    pub sites: Option<Vec<String>>,
    /// Restrict to these machines (each must already be permitted).
    #[serde(default)]
    pub machines: Option<Vec<String>>,
}

/// A narrowing request tried to step outside the current scope.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ScopeError {
    /// Requested site is not in the current scope.
    #[error("site '{0}' is outside the current scope")]
    SiteOutOfScope(String),
    /// Requested machine is unknown or not in the current scope (indistinguishable on purpose).
    #[error("machine '{0}' is outside the current scope")]
    MachineOutOfScope(String),
}

impl Scope {
    /// Build a scope.
    pub fn new<S: Into<String>>(
        role: Role,
        sites: impl IntoIterator<Item = S>,
        machines: Option<Vec<String>>,
    ) -> Self {
        Self {
            role,
            sites: sites.into_iter().map(Into::into).collect(),
            machines: machines.map(|m| m.into_iter().collect()),
        }
    }

    /// The role.
    #[must_use]
    pub fn role(&self) -> Role {
        self.role
    }

    /// Permitted sites.
    #[must_use]
    pub fn sites(&self) -> &BTreeSet<String> {
        &self.sites
    }

    /// Permitted machines, or `None` for "all machines on permitted sites".
    #[must_use]
    pub fn machines(&self) -> Option<&BTreeSet<String>> {
        self.machines.as_ref()
    }

    /// Whether `machine` on `site` is visible.
    #[must_use]
    pub fn permits(&self, site: &str, machine: &str) -> bool {
        self.sites.contains(site) && self.machines.as_ref().is_none_or(|m| m.contains(machine))
    }

    /// Structural subset check: `self` grants nothing that `other` does not.
    #[must_use]
    pub fn is_subset_of(&self, other: &Scope) -> bool {
        self.role == other.role
            && self.sites.is_subset(&other.sites)
            && match (&self.machines, &other.machines) {
                (_, None) => true,
                (Some(mine), Some(theirs)) => mine.is_subset(theirs),
                (None, Some(_)) => false,
            }
    }

    /// Derive a narrower scope. Never widens.
    ///
    /// * `sites: None` keeps the current sites; `Some(list)` requires `list ⊆ sites`.
    /// * `machines: None` keeps the current machine set; `Some(list)` requires every
    ///   machine to exist in `catalog` and be permitted by `self`.
    ///
    /// Requesting site X and a machine on site Y (both permitted) is accepted and simply
    /// yields an intersection that shows nothing for that machine.
    ///
    /// # Errors
    ///
    /// [`ScopeError`] naming the first out-of-scope site or machine. On error no scope is
    /// produced, so a rejected request can never be partially applied.
    pub fn narrow(&self, request: &ScopeRequest, catalog: &Catalog) -> Result<Scope, ScopeError> {
        let sites = match &request.sites {
            // Clone: the child owns its sets so the parent (session scope) stays usable.
            None => self.sites.clone(),
            Some(requested) => {
                if let Some(bad) = requested.iter().find(|s| !self.sites.contains(*s)) {
                    return Err(ScopeError::SiteOutOfScope(bad.clone()));
                }
                requested.iter().cloned().collect()
            }
        };
        let machines = match &request.machines {
            None => self.machines.clone(),
            Some(requested) => {
                let permitted =
                    |m: &String| catalog.site_of(m).is_some_and(|site| self.permits(site, m));
                if let Some(bad) = requested.iter().find(|m| !permitted(m)) {
                    return Err(ScopeError::MachineOutOfScope(bad.clone()));
                }
                Some(requested.iter().cloned().collect())
            }
        };
        Ok(Scope {
            role: self.role,
            sites,
            machines,
        })
    }
}

/// An authenticated caller and its starting scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    /// Principal name, for attribution.
    pub name: String,
    /// Starting (widest) scope for the session.
    pub scope: Scope,
}

/// Names of the built-in synthetic principals.
pub const SYNTHETIC_PRINCIPALS: [&str; 3] =
    ["viewer-all", "operator-site-alpha", "operator-line-b1"];

/// Resolve a synthetic token to a principal. Unknown tokens resolve to `None` and the
/// server refuses to start (fail closed); there is no default principal.
#[must_use]
pub fn lookup_principal(token: &str) -> Option<Principal> {
    let scope = match token {
        "viewer-all" => Scope::new(
            Role::Viewer,
            ["site-alpha", "site-beta", "site-gamma"],
            None,
        ),
        "operator-site-alpha" => Scope::new(Role::Operator, ["site-alpha"], None),
        "operator-line-b1" => Scope::new(
            Role::Operator,
            ["site-beta"],
            Some(vec!["line-b1".to_owned()]),
        ),
        _ => return None,
    };
    Some(Principal {
        name: token.to_owned(),
        scope,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITES: [&str; 3] = ["site-alpha", "site-beta", "site-gamma"];

    fn subset_of_sites(mask: u8) -> Vec<String> {
        SITES
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, s)| (*s).to_owned())
            .collect()
    }

    fn sites_req(sites: &[&str]) -> ScopeRequest {
        ScopeRequest {
            sites: Some(sites.iter().map(|s| (*s).to_owned()).collect()),
            machines: None,
        }
    }

    fn machines_req(machines: &[&str]) -> ScopeRequest {
        ScopeRequest {
            sites: None,
            machines: Some(machines.iter().map(|s| (*s).to_owned()).collect()),
        }
    }

    /// Semantic subset over the whole catalog, independent of the structural check.
    fn semantically_within(child: &Scope, parent: &Scope, catalog: &Catalog) -> bool {
        catalog
            .machines()
            .iter()
            .all(|m| !child.permits(&m.site, &m.id) || parent.permits(&m.site, &m.id))
    }

    #[test]
    fn exhaustive_site_narrowing_over_three_site_universe() {
        let catalog = Catalog::synthetic();
        for parent_mask in 0u8..8 {
            let parent = Scope::new(Role::Viewer, subset_of_sites(parent_mask), None);
            for req_mask in 0u8..8 {
                let request = ScopeRequest {
                    sites: Some(subset_of_sites(req_mask)),
                    machines: None,
                };
                let result = parent.narrow(&request, &catalog);
                let is_subset_request = req_mask & !parent_mask == 0;
                if is_subset_request {
                    let child = result.unwrap();
                    assert!(
                        child.is_subset_of(&parent),
                        "{parent_mask:03b}/{req_mask:03b}"
                    );
                    assert!(semantically_within(&child, &parent, &catalog));
                    let expected: BTreeSet<String> =
                        subset_of_sites(req_mask).into_iter().collect();
                    assert_eq!(child.sites(), &expected);
                } else {
                    assert!(
                        matches!(result, Err(ScopeError::SiteOutOfScope(_))),
                        "escalation {req_mask:03b} from {parent_mask:03b} must be rejected"
                    );
                }
            }
            // No request at all is the identity.
            assert_eq!(
                parent.narrow(&ScopeRequest::default(), &catalog).unwrap(),
                parent
            );
        }
    }

    #[test]
    fn narrowing_is_transitive_and_never_widens() {
        let catalog = Catalog::synthetic();
        let root = lookup_principal("viewer-all").unwrap().scope;
        let a = root
            .narrow(&sites_req(&["site-alpha", "site-beta"]), &catalog)
            .unwrap();
        let b = a
            .narrow(&machines_req(&["line-a1", "line-b1"]), &catalog)
            .unwrap();
        let c = b.narrow(&machines_req(&["line-b1"]), &catalog).unwrap();
        for (child, parent) in [(&a, &root), (&b, &a), (&c, &b), (&c, &root)] {
            assert!(child.is_subset_of(parent));
            assert!(semantically_within(child, parent, &catalog));
        }
        // Once narrowed to line-b1, asking for line-a1 again is an escalation.
        assert_eq!(
            c.narrow(&machines_req(&["line-a1"]), &catalog),
            Err(ScopeError::MachineOutOfScope("line-a1".to_owned()))
        );
    }

    #[test]
    fn escalation_attempts_are_rejected() {
        let catalog = Catalog::synthetic();
        let alpha = lookup_principal("operator-site-alpha").unwrap().scope;
        assert_eq!(
            alpha.narrow(&sites_req(&["site-alpha", "site-beta"]), &catalog),
            Err(ScopeError::SiteOutOfScope("site-beta".to_owned()))
        );
        assert_eq!(
            alpha.narrow(&machines_req(&["line-b1"]), &catalog),
            Err(ScopeError::MachineOutOfScope("line-b1".to_owned()))
        );
        // Unknown machines are rejected the same way as forbidden ones.
        assert_eq!(
            alpha.narrow(&machines_req(&["line-zz"]), &catalog),
            Err(ScopeError::MachineOutOfScope("line-zz".to_owned()))
        );

        let b1 = lookup_principal("operator-line-b1").unwrap().scope;
        assert_eq!(
            b1.narrow(&machines_req(&["line-b2"]), &catalog),
            Err(ScopeError::MachineOutOfScope("line-b2".to_owned()))
        );
    }

    #[test]
    fn cross_site_machine_request_yields_empty_intersection_not_widening() {
        let catalog = Catalog::synthetic();
        let root = lookup_principal("viewer-all").unwrap().scope;
        let req = ScopeRequest {
            sites: Some(vec!["site-alpha".to_owned()]),
            machines: Some(vec!["line-b1".to_owned()]),
        };
        let child = root.narrow(&req, &catalog).unwrap();
        assert!(catalog
            .machines()
            .iter()
            .all(|m| !child.permits(&m.site, &m.id)));
    }

    #[test]
    fn unknown_principal_fails_closed() {
        assert!(lookup_principal("").is_none());
        assert!(lookup_principal("admin").is_none());
        for name in SYNTHETIC_PRINCIPALS {
            assert!(lookup_principal(name).is_some());
        }
    }
}
