//! Items this computer keeps to itself: never sent to the NAS, and what the
//! NAS has under those names is not shown here (like `.stignore` in
//! Syncthing). Typical use: `.git`, `node_modules`, `*.tmp`.
//!
//! Pattern syntax (a small subset of `.gitignore`), always case-insensitive:
//!
//! * `node_modules` matches a file or folder with that name, at any depth;
//! * `*.log`, `cache-?` use `*` (any run of characters) and `?` (one);
//! * `build/` (trailing slash) matches folders only;
//! * `/dist` or `site/out` (a slash before the end) is a path from the top of
//!   the NAS folder; `**` there stands for any number of folders.
//!
//! When a folder matches, everything inside it is excluded too.

#[derive(Clone, Debug, Default)]
pub struct Excludes {
    rules: Vec<Rule>,
}

#[derive(Clone, Debug)]
struct Rule {
    /// Lower-cased segments; a single one for unanchored rules.
    segs: Vec<String>,
    anchored: bool,
    dir_only: bool,
}

impl Excludes {
    pub fn new<S: AsRef<str>>(patterns: &[S]) -> Self {
        let rules = patterns.iter().filter_map(|p| Rule::parse(p.as_ref())).collect();
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `rel` (relative to the NAS folder, `/`-separated) is excluded,
    /// itself or through one of the folders it is in. `is_dir` says what the
    /// last component is; the ones before it are folders.
    pub fn matches(&self, rel: &str, is_dir: bool) -> bool {
        if self.rules.is_empty() {
            return false;
        }
        let parts: Vec<String> = rel.split('/').filter(|s| !s.is_empty()).map(str::to_lowercase).collect();
        (1..=parts.len()).any(|depth| {
            let dir = depth < parts.len() || is_dir;
            self.rules.iter().any(|r| r.matches(&parts[..depth], dir))
        })
    }
}

impl Rule {
    fn parse(pattern: &str) -> Option<Self> {
        let p = pattern.trim().replace('\\', "/");
        let dir_only = p.ends_with('/');
        let p = p.trim_end_matches('/');
        let anchored = p.contains('/');
        let segs: Vec<String> = p.split('/').filter(|s| !s.is_empty() && *s != ".").map(str::to_lowercase).collect();
        if segs.is_empty() || segs.iter().any(|s| s == "..") || (!anchored && segs[0] == "**") {
            return None;
        }
        Some(Self { segs, anchored, dir_only })
    }

    /// Whether the rule matches exactly this path (not one of its parents).
    fn matches(&self, parts: &[String], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        if self.anchored {
            segments(&self.segs, parts)
        } else {
            parts.last().is_some_and(|name| glob(&self.segs[0], name))
        }
    }
}

/// `pat` against `parts`, segment by segment, `**` covering any number of them.
fn segments(pat: &[String], parts: &[String]) -> bool {
    match pat.split_first() {
        None => parts.is_empty(),
        Some((p, rest)) if p == "**" => (0..=parts.len()).any(|skip| segments(rest, &parts[skip..])),
        Some((p, rest)) => parts.split_first().is_some_and(|(name, more)| glob(p, name) && segments(rest, more)),
    }
}

/// `*` and `?` wildcards within one name.
fn glob(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ex(patterns: &[&str]) -> Excludes {
        Excludes::new(patterns)
    }

    #[test]
    fn names_at_any_depth() {
        let e = ex(&["node_modules", ".git"]);
        assert!(e.matches("node_modules", true));
        assert!(e.matches("proj/node_modules", true));
        assert!(e.matches("proj/node_modules/react/index.js", false));
        assert!(e.matches("a/b/.git/HEAD", false));
        assert!(e.matches("proj/.git", false)); // a submodule's .git is a file
        assert!(!e.matches("proj/src/index.js", false));
        assert!(!e.matches("proj/node_modules2", true));
        assert!(!e.matches("", true));
        assert!(Excludes::default().is_empty());
        assert!(!Excludes::default().matches("node_modules", true));
    }

    #[test]
    fn wildcards_and_case() {
        let e = ex(&["*.tmp", "cache-?", "~$*"]);
        assert!(e.matches("docs/x.TMP", false));
        assert!(e.matches("a.tmp/inside.txt", false)); // a folder called a.tmp
        assert!(e.matches("cache-1", true));
        assert!(!e.matches("cache-10", true));
        assert!(e.matches("~$report.docx", false));
        assert!(!e.matches("report.tmpl", false));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
        assert!(glob("*", ""));
    }

    #[test]
    fn folders_only() {
        let e = ex(&["build/"]);
        assert!(e.matches("build", true));
        assert!(e.matches("x/build/out.o", false));
        assert!(!e.matches("x/build", false));
    }

    #[test]
    fn anchored_paths() {
        let e = ex(&["/dist", "site/out/", "src/**/gen"]);
        assert!(e.matches("dist", true));
        assert!(e.matches("dist/app.js", false));
        assert!(!e.matches("x/dist", true));
        assert!(e.matches("site/out/index.html", false));
        assert!(!e.matches("site/out", false));
        assert!(!e.matches("other/site/out", true));
        assert!(e.matches("src/gen", true));
        assert!(e.matches("src/a/b/gen/x.rs", false));
        assert!(!e.matches("src/a/b/generated", true));
    }

    #[test]
    fn junk_patterns_are_ignored() {
        let e = ex(&["", "  ", "/", "../x", "**", "\\windows\\style\\"]);
        assert_eq!(e.rules.len(), 1);
        assert!(e.matches("windows/style", true));
        assert!(!e.matches("anything", false));
    }
}
