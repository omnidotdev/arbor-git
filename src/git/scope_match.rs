//! Ref/path glob scope matching for the push credential boundary.
//!
//! A faithful port of arbor-api's `lib/auth/refPathMatch.ts` and
//! `lib/git/receivePackGuard.ts`, so a confined token means exactly the same
//! thing when a push lands here as it does at arbor-api's GraphQL and MCP gates.
//! The glob semantics are deliberately small and identical wherever a pattern is
//! checked:
//!
//! - `*` matches any run of characters within a single segment (never a `/`)
//! - `**` matches any run of characters across segments (including `/` and empty)
//! - every other character, including regex metacharacters, is literal
//!
//! The tests carry a differential corpus whose expected verdicts are the TS
//! matcher's, so the two implementations cannot drift without a test failing.

/// Whether a single glob pattern matches a value in full.
pub fn matches_glob(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    glob_match(&p, &v)
}

/// Anchored recursive match. `*` consumes within a segment (stops at `/`); `**`
/// consumes across segments; any other char is literal.
fn glob_match(p: &[char], v: &[char]) -> bool {
    if p.is_empty() {
        return v.is_empty();
    }

    if p[0] == '*' {
        if p.len() >= 2 && p[1] == '*' {
            // `**`: match any run, including `/`
            let rest = &p[2..];
            return (0..=v.len()).any(|k| glob_match(rest, &v[k..]));
        }

        // `*`: match any run within the current segment (never crossing `/`)
        let rest = &p[1..];
        let mut k = 0;
        loop {
            if glob_match(rest, &v[k..]) {
                return true;
            }
            if k < v.len() && v[k] != '/' {
                k += 1;
            } else {
                return false;
            }
        }
    }

    if !v.is_empty() && p[0] == v[0] {
        return glob_match(&p[1..], &v[1..]);
    }

    false
}

/// Whether any glob in the list matches the value. An empty list matches nothing,
/// so a caller narrowed to zero patterns fails closed.
pub fn matches_any_glob(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| matches_glob(pattern, value))
}

/// A ref update and the repo-relative paths its new objects change.
#[derive(Debug, Clone)]
pub struct RefUpdate {
    pub reference: String,
    /// Paths the update introduces; empty for a deletion.
    pub changed_paths: Vec<String>,
}

/// The confinement for the repository being pushed to. `None` in a dimension
/// means unconfined there (every ref / every path).
#[derive(Debug, Clone)]
pub struct ScopeBounds {
    pub ref_patterns: Option<Vec<String>>,
    pub path_patterns: Option<Vec<String>>,
}

/// A single ref update the boundary refuses, with a client-facing reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRejection {
    pub reference: String,
    pub reason: String,
}

/// Whether a ref is within the bounds (`None` patterns = every ref).
fn ref_allowed(bounds: &ScopeBounds, reference: &str) -> bool {
    bounds
        .ref_patterns
        .as_ref()
        .is_none_or(|patterns| matches_any_glob(patterns, reference))
}

/// The first changed path outside the bounds, or `None` when all are allowed.
fn first_forbidden_path<'a>(bounds: &ScopeBounds, changed_paths: &'a [String]) -> Option<&'a str> {
    let patterns = bounds.path_patterns.as_ref()?;
    changed_paths
        .iter()
        .find(|path| !matches_any_glob(patterns, path))
        .map(String::as_str)
}

/// Decide which ref updates a confined credential may not make.
///
/// Each update is judged independently: its ref must be in bounds, and every path
/// its new objects change must be in bounds. A deletion carries no changed paths,
/// so it is judged on its ref alone. Returns one rejection per refused update
/// (empty means the whole push is allowed).
pub fn evaluate_receive_pack(bounds: &ScopeBounds, updates: &[RefUpdate]) -> Vec<UpdateRejection> {
    let mut rejections = Vec::new();

    for update in updates {
        if !ref_allowed(bounds, &update.reference) {
            rejections.push(UpdateRejection {
                reference: update.reference.clone(),
                reason: format!("ref {} is outside this token's scope", update.reference),
            });
            continue;
        }

        if let Some(forbidden) = first_forbidden_path(bounds, &update.changed_paths) {
            rejections.push(UpdateRejection {
                reference: update.reference.clone(),
                reason: format!("path {forbidden} is outside this token's scope"),
            });
        }
    }

    rejections
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Differential corpus: each expected verdict is what arbor-api's
    /// `matchesGlob` (refPathMatch.ts) returns for the same inputs. If the Rust
    /// matcher ever diverges from the TS one, a row here fails, which is the whole
    /// point - a confined token must mean the same thing on both sides.
    #[test]
    fn matches_glob_agrees_with_the_ts_matcher() {
        let cases: &[(&str, &str, bool)] = &[
            // `*` matches within a single segment, never across `/`
            ("refs/heads/agent/*", "refs/heads/agent/feature", true),
            ("refs/heads/agent/*", "refs/heads/agent/a/b", false),
            ("refs/heads/agent/*", "refs/heads/agent/", true),
            ("refs/heads/*", "refs/heads/main", true),
            ("refs/heads/*", "refs/tags/v1", false),
            ("*", "anything", true),
            ("*", "a/b", false),
            ("a*b", "aXXb", true),
            ("a*b", "aX/Xb", false),
            ("src/*.ts", "src/index.ts", true),
            ("src/*.ts", "src/lib/index.ts", false),
            // `**` matches across segments, including empty
            ("**", "refs/heads/anything/deep", true),
            ("src/**", "src/a/b/c.ts", true),
            ("src/**", "src/", true),
            ("src/**", "src", false),
            ("**/*.ts", "a/b/c.ts", true),
            ("**/*.ts", "c.ts", false),
            // every other character is literal (metacharacters included)
            ("v1.0", "v1.0", true),
            ("v1.0", "v1x0", false),
            ("", "", true),
            ("", "x", false),
        ];

        for (pattern, value, expected) in cases {
            assert_eq!(
                matches_glob(pattern, value),
                *expected,
                "matches_glob({pattern:?}, {value:?}) should be {expected}"
            );
        }
    }

    #[test]
    fn matches_any_glob_fails_closed_on_an_empty_list() {
        assert!(matches_any_glob(
            &["refs/heads/main".into(), "refs/heads/dev".into()],
            "refs/heads/dev"
        ));
        assert!(!matches_any_glob(&["a".into(), "b".into()], "c"));
        // an empty list matches nothing, so a narrowed-to-zero caller fails closed
        assert!(!matches_any_glob(&[], "anything"));
    }

    #[test]
    fn evaluate_receive_pack_judges_each_update() {
        let update = |reference: &str, paths: &[&str]| RefUpdate {
            reference: reference.into(),
            changed_paths: paths.iter().map(|p| (*p).into()).collect(),
        };

        // unconfined: nothing is refused
        let unconfined = ScopeBounds {
            ref_patterns: None,
            path_patterns: None,
        };
        assert!(
            evaluate_receive_pack(&unconfined, &[update("refs/heads/main", &["any/where.rs"])])
                .is_empty()
        );

        // ref confinement: a ref outside the globs is refused
        let ref_confined = ScopeBounds {
            ref_patterns: Some(vec!["refs/heads/agent/*".into()]),
            path_patterns: None,
        };
        let rejected = evaluate_receive_pack(&ref_confined, &[update("refs/heads/main", &[])]);
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].reason.contains("outside this token's scope"));

        // path confinement: an in-bounds ref with an out-of-bounds path is refused
        let path_confined = ScopeBounds {
            ref_patterns: Some(vec!["refs/heads/agent/*".into()]),
            path_patterns: Some(vec!["src/**".into()]),
        };
        let rejected = evaluate_receive_pack(
            &path_confined,
            &[update("refs/heads/agent/x", &["src/a.rs", "lib/b.rs"])],
        );
        assert_eq!(rejected.len(), 1);
        assert!(rejected[0].reason.contains("lib/b.rs"));

        // in-bounds ref and paths: allowed
        assert!(
            evaluate_receive_pack(
                &path_confined,
                &[update("refs/heads/agent/x", &["src/a.rs", "src/deep/b.rs"])]
            )
            .is_empty()
        );

        // a deletion carries no paths, so it is judged on its ref alone
        assert!(
            evaluate_receive_pack(&path_confined, &[update("refs/heads/agent/x", &[])]).is_empty()
        );
    }
}
