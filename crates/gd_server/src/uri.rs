//! `file://` URI ↔ filesystem path conversion at the LSP boundary.
//!
//! LSP clients address documents with `file://` URIs; the indexer and VFS work in absolute paths. This
//! is a deliberately small, dependency-free converter: strip the `file://` scheme, percent-decode
//! the path, drop the leading slash before a Windows drive letter (`file:///C:/x` → `C:/x`), and
//! reconstruct a UNC path when the URI carries a host authority (`file://server/share/x` →
//! `//server/share/x`, RFC 8089 §E.3) instead of silently dropping the host. Anything that isn't a
//! usable `file://` path yields `None`, and the caller degrades (skip the index update / disk read)
//! rather than guessing — never crash, never lie.
//!
//! The reverse direction ([`path_to_file_uri`]) percent-encodes any character that would otherwise
//! make `Uri::from_str` reject the result — spaces, `#`, `?`, and other unreserved-set escapes.
//! Without this, a project at `C:\Users\Alice\My Game\` produces an unparseable URI and every nav
//! handler silently drops the file, returning empty results from `workspace/symbol`, `references`,
//! `implementation`, both `callHierarchy/*` handlers, and the global-class `definition` path.

use std::cell::RefCell;
use std::num::NonZeroUsize;

use camino::Utf8PathBuf;
use lru::LruCache;
use lsp_types::Uri;

thread_local! {
    /// Per-thread memo for [`dunce::canonicalize`] results, keyed by the exact input path.
    /// [`CanonicalKey::for_path`] is on the per-request hot path — the WP-R2 cross-file xref query
    /// (`xfile::member_initializer_xrefs`) fans it out per candidate, and every nav handler re-keys
    /// its document — and each cold call is a filesystem syscall (`GetFinalPathNameByHandle` on
    /// Windows). Caching the *successful* resolutions collapses that to one syscall per distinct
    /// path for the session. Only successes are cached: a not-yet-created / in-memory path must
    /// re-resolve once it lands on disk, and a failing canonicalize is cheap anyway. Staleness
    /// window: a path whose on-disk identity is repointed mid-session (a junction retargeted) keeps
    /// its first resolution — negligible, and no worse than the content-hash-keyed parse/analysis
    /// caches this feeds, which re-validate independently.
    static CANONICALIZE_MEMO: RefCell<LruCache<Utf8PathBuf, Utf8PathBuf>> = RefCell::new(
        LruCache::new(NonZeroUsize::new(4096).expect("invariant: 4096 is nonzero")),
    );
}

/// [`dunce::canonicalize`], memoized per thread (see [`CANONICALIZE_MEMO`]). Returns the canonical
/// path on success, `None` on any error (the caller falls back to the input path, exactly as the
/// un-memoized call did).
fn canonicalize_memoized(path: &camino::Utf8Path) -> Option<Utf8PathBuf> {
    CANONICALIZE_MEMO.with(|cell| {
        let mut memo = cell.borrow_mut();
        if let Some(hit) = memo.get(path) {
            return Some(hit.clone());
        }
        let resolved = dunce::canonicalize(path.as_std_path())
            .ok()
            .and_then(|pb| Utf8PathBuf::from_path_buf(pb).ok())?;
        memo.put(path.to_owned(), resolved.clone());
        Some(resolved)
    })
}

/// Convert a `file://` URI to an absolute filesystem path, or `None` if it isn't one we can map.
pub fn uri_to_path(uri: &Uri) -> Option<Utf8PathBuf> {
    let rest = uri.as_str().strip_prefix("file://")?;
    // `rest` is `<authority><path>`; the path starts at the first '/'. A `file:///x` URI has an
    // empty authority, so the path is the whole of `rest` from index 0.
    let slash = rest.find('/')?;
    let authority = &rest[..slash];
    let decoded = percent_decode(&rest[slash..]);
    // A non-empty authority is a UNC host: `file://server/share/x` (RFC 8089 §E.3 — the form VS
    // Code emits for a project on a network share). Reconstruct the UNC path `//server/share/x`
    // (camino/Windows treat `\\server\share` ↔ `//server/share`) instead of silently dropping the
    // host and returning the wrong, host-less `/share/x`, which made every nav handler resolve
    // against a nonexistent file on a network-share project. `localhost` is the RFC's alias for
    // "this machine", so treat it as an empty authority (an ordinary local path).
    if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
        let host = percent_decode(authority);
        return Some(Utf8PathBuf::from(format!("//{host}{decoded}")));
    }
    Some(Utf8PathBuf::from(strip_windows_drive_slash(&decoded)))
}

/// Convert an absolute filesystem path to a `file://` URI. Percent-encodes every byte outside the
/// URI-safe unreserved set so paths containing spaces, `#`, `?`, etc. produce a parseable URI.
///
/// Returns `None` only when the resulting string fails [`Uri`] parsing — a defensive guard
/// against pathological inputs (e.g. a control character `Utf8Path` happens to allow). The
/// common cases (spaces in `My Game`, `#`/`?` in vendored content) all round-trip through
/// [`uri_to_path`].
pub fn path_to_file_uri(path: &camino::Utf8Path) -> Option<Uri> {
    let normalized = path.as_str().replace('\\', "/");
    let encoded = percent_encode_path(&normalized);
    let with_root = if encoded.starts_with("//") {
        // UNC path `//server/share/x` → `file://server/share/x` (the host rides in the URI
        // authority, RFC 8089 §E.3). Inverse of `uri_to_path`'s UNC reconstruction.
        format!("file:{encoded}")
    } else if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
    };
    with_root.parse().ok()
}

/// Canonical cache-key form for `path`: the string that [`path_to_file_uri`] produces for the
/// same path. Private — the public chokepoint is [`CanonicalKey`], whose [`CanonicalKey::for_path`]
/// is the only caller; keeping this string-returning form out of the public API stops a caller from
/// keying a cache with a bare percent-encoded string and re-introducing the raw-vs-encoded drift
/// the newtype exists to prevent (the `My Game`/`%20` WP-R2 regression).
///
/// Same `None` semantics as [`path_to_file_uri`] — pathological paths that fail [`Uri`]
/// parsing degrade rather than panic.
fn canonical_key(path: &camino::Utf8Path) -> Option<String> {
    path_to_file_uri(path).map(|u| u.as_str().to_string())
}

/// The sole key type for the per-file parse and analysis caches ([`crate::workspace::Workspace`]).
///
/// It carries the canonical percent-encoded `file://` URI string. Constructing one *only* through
/// [`Self::for_uri`] (an LSP wire URI) or [`Self::for_path`] (a filesystem path, routed through
/// the private `canonical_key` helper) makes it a compile-time impossibility to key the cache with a raw
/// `path.as_str()` or a differently-encoded URI. That drift is exactly what silently disabled
/// WP-R2 cross-file cycle detection for any project living under a path with a space (the
/// `My Game` bug): the writer keyed on the percent-encoded wire URI while the reader probed a raw
/// path string, so the two never met. The type makes the two halves agree by construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CanonicalKey(String);

impl CanonicalKey {
    /// The cache key for an LSP wire URI. The client-sent [`Uri`] is already a `file://` URI, but its
    /// *exact bytes* are the client's, not ours: `lsp-types` 0.97 parses through `fluent-uri` 0.1.4,
    /// which does **not** re-normalize percent-encoding, so [`Uri::as_str`] preserves whatever the
    /// client wrote — including sub-delimiters a client may leave raw that our own encoder escapes
    /// (`+ ( ) , ; = & $ ! ' [ ] { }`; see [`is_path_safe`]). Keying off those raw bytes would split
    /// the cache from [`Self::for_path`], which re-encodes the disk path: e.g. a client URI
    /// `file:///proj/Main(old).gd` keyed verbatim here but `…Main%28old%29.gd` on the reader side,
    /// silently disabling WP-R2 cross-file cycle detection (and serving stale/missing diagnostics) for
    /// those files. So we route the URI back to a path via [`uri_to_path`] and re-key it through the
    /// *same* [`Self::for_path`] pipeline the reader uses, making the two halves agree by construction
    /// regardless of the client's encoding or (via [`fold_uri_drive`], applied inside `for_path`)
    /// Windows drive casing. A non-`file://` URI (`untitled:`, `https:`) or a path the encoder rejects
    /// can't round-trip; we fall back to the drive-folded wire string so the caller still gets a
    /// stable key (these never reach the disk-keyed reader, so there is nothing to agree with).
    pub fn for_uri(uri: &Uri) -> Self {
        uri_to_path(uri)
            .and_then(|p| Self::for_path(&p))
            .unwrap_or_else(|| CanonicalKey(fold_uri_drive(uri.as_str())))
    }

    /// The cache key for a filesystem path: the string [`path_to_file_uri`] produces, drive-folded
    /// to upper via [`fold_uri_drive`] so it equals the [`Self::for_uri`] key for the same file
    /// regardless of which side's drive casing reached the cache first. A path-derived lookup (the
    /// cross-file xref query in `xfile.rs`, the watcher's evict-on-reindex path) thus hits the entry
    /// the URI-keyed writer stored. `None` for the pathological paths [`path_to_file_uri`] rejects —
    /// the caller degrades to "no cache entry" rather than guessing.
    ///
    /// WP-RD9: the path is first run through [`dunce::canonicalize`] (junction / 8.3 short name /
    /// symlink resolution, recovering the real on-disk component case), so a file reached through a
    /// junction or a differently-cased path keys to the same cache entry as its real path. The
    /// canonicalization is best-effort: a not-yet-created / in-memory path errors and falls back to
    /// the input, where the writer and reader still agree on the un-canonicalized form. Because
    /// [`Self::for_uri`] routes through here too, both halves canonicalize identically.
    ///
    /// The canonicalize syscall is memoized per thread (see [`canonicalize_memoized`]) so this hot
    /// per-request path doesn't `stat` the disk on every call; the memo is a transparent
    /// accelerator and never changes the key this returns.
    pub fn for_path(path: &camino::Utf8Path) -> Option<Self> {
        let canonical = canonicalize_memoized(path);
        let resolved = canonical.as_deref().unwrap_or(path);
        canonical_key(resolved).map(|k| CanonicalKey(fold_uri_drive(&k)))
    }

    /// The underlying canonical URI string (for logging / VFS lookups that are still string-keyed).
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Percent-encode every byte outside the URI unreserved set + path-safe characters
/// (`/`, `:`, `@`). Spaces / `#` / `?` / `&` are the high-impact ones; the rest are
/// belt-and-suspenders against pathological filenames. ASCII-only inputs are encoded byte-by-byte;
/// non-ASCII bytes are UTF-8 sequences which we encode as `%XX` per byte (the symmetric inverse
/// of `percent_decode`'s lossy UTF-8 reconstitution).
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if is_path_safe(b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0F) as usize] as char);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Bytes that don't need percent-escaping in a `file://` URI path. Conservative: the URI
/// unreserved set (RFC 3986 §2.3) plus the path-safe punctuation `/` `:` `@`. Drive-letter
/// colons round-trip; backslashes were already converted to forward slashes by the caller.
fn is_path_safe(b: u8) -> bool {
    matches!(b,
        b'A'..=b'Z'
        | b'a'..=b'z'
        | b'0'..=b'9'
        | b'-' | b'_' | b'.' | b'~'
        | b'/' | b':' | b'@'
    )
}

/// `/C:/x` → `C:/x` on Windows (a leading slash before a `drive:` is a URI artifact). No-op elsewhere
/// and for non-drive paths.
fn strip_windows_drive_slash(path: &str) -> &str {
    if cfg!(windows) {
        let b = path.as_bytes();
        if b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':' {
            return &path[1..];
        }
    }
    path
}

/// Upper-case the drive letter in a `file:///c:/…` URI string so a key built from a client wire URI
/// (VS Code lower-cases the drive) equals one built from a disk-walk path (`path_to_file_uri`, whose
/// drive follows the project-root casing — upper in practice). The URI-side companion of
/// `gd_project::normalize`'s drive fold; keeps the analysis cache's URI-keyed writer and its
/// path-keyed cross-file reader in agreement. No-op for non-`file:///<letter>:` URIs (UNC, POSIX),
/// and allocates only when it actually folds.
fn fold_uri_drive(uri: &str) -> String {
    const PREFIX: &str = "file:///";
    if let Some(rest) = uri.strip_prefix(PREFIX) {
        let rb = rest.as_bytes();
        if rb.len() >= 2 && rb[0].is_ascii_lowercase() && rb[1] == b':' {
            let mut out = String::with_capacity(uri.len());
            out.push_str(PREFIX);
            out.push((rb[0] as char).to_ascii_uppercase());
            out.push_str(&rest[1..]);
            return out;
        }
    }
    uri.to_string()
}

/// Decode `%XX` escapes; leaves any malformed escape verbatim. UTF-8 lossy on the decoded bytes.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn decodes_spaces() {
        let p = uri_to_path(&uri("file:///tmp/my%20project/a.gd")).unwrap();
        assert!(p.as_str().contains("my project/a.gd"));
    }

    #[test]
    fn non_file_scheme_is_none() {
        assert!(uri_to_path(&uri("untitled:Untitled-1")).is_none());
        assert!(uri_to_path(&uri("https://example.com/x")).is_none());
    }

    #[test]
    fn path_to_file_uri_encodes_spaces() {
        let p = camino::Utf8PathBuf::from("/tmp/My Game/foo bar.gd");
        let u = path_to_file_uri(&p).expect("space-containing path should produce a valid URI");
        // The URI string itself must NOT contain a raw space (would be a parse error).
        assert!(!u.as_str().contains(' '), "spaces must be percent-encoded");
        // Round-trip back to a path: the spaces decode.
        let back = uri_to_path(&u).unwrap();
        assert!(back.as_str().contains("My Game/foo bar.gd"));
    }

    #[test]
    fn canonical_key_matches_path_to_file_uri_to_string() {
        // The chokepoint contract: canonical_key(p) is exactly what `path_to_file_uri(p)`
        // produces when its result is stringified. Both the writer (Workspace::analyze) and
        // the reader (xfile::cache_keys) MUST agree on this so the WP-R2 cycle check fires
        // on space-containing project paths.
        let p = camino::Utf8PathBuf::from("/tmp/My Game/sub/foo.gd");
        let u = path_to_file_uri(&p).unwrap();
        let k = canonical_key(&p).unwrap();
        assert_eq!(k, u.as_str());
        assert!(
            k.contains("%20"),
            "canonical_key must percent-encode spaces"
        );
        assert!(
            !k.contains(' '),
            "canonical_key must not contain raw spaces"
        );
    }

    #[test]
    fn for_path_memoizes_canonicalize_without_changing_the_key() {
        // A real on-disk file so `dunce::canonicalize` succeeds and the result is memoized. The
        // second call must hit the memo and produce an identical key — the memo is a transparent
        // accelerator, never a behavior change. (A non-existent path canonicalizes to an Err and is
        // deliberately not cached, so this uses a materialised file.)
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hero.gd");
        std::fs::write(&file, "extends Node\n").unwrap();
        let p = camino::Utf8PathBuf::from_path_buf(file).unwrap();
        let first = CanonicalKey::for_path(&p).expect("existing file canonicalizes");
        let second = CanonicalKey::for_path(&p).expect("existing file canonicalizes");
        assert_eq!(
            first, second,
            "the canonicalize memo must not change the key across calls"
        );
        // And the path-keyed reader still agrees with the URI-keyed writer for the same file.
        let from_uri = CanonicalKey::for_uri(&path_to_file_uri(&p).unwrap());
        assert_eq!(from_uri, first);
    }

    #[test]
    fn canonical_key_for_uri_and_for_path_agree() {
        // The whole point of the newtype: the URI-keyed writer (`for_uri`, what `didOpen`
        // diagnostics store under) and the path-keyed reader (`for_path`, what the xfile xref
        // query and the watcher eviction derive) must produce the SAME key for the same file —
        // including under a space-containing project path, the regression that disabled WP-R2.
        let path = camino::Utf8PathBuf::from("/proj/My Game/sub/foo.gd");
        let wire = path_to_file_uri(&path).unwrap();
        let from_uri = CanonicalKey::for_uri(&wire);
        let from_path = CanonicalKey::for_path(&path).unwrap();
        assert_eq!(from_uri, from_path);
        assert!(from_uri.as_str().contains("%20"));
        assert!(!from_uri.as_str().contains(' '));
    }

    #[test]
    fn canonical_key_folds_drive_case_so_client_and_disk_agree() {
        // VS Code lower-cases the drive in wire URIs (`file:///c:/…`); the
        // disk walk keeps the root's upper case (`C:/…`). The xref reader keys from the index (disk)
        // path while the diagnostics writer keys from the wire URI — without a drive fold they never
        // meet and WP-R2 cross-file cycle detection silently went inert on Windows. The fold makes
        // the two keys equal.
        let from_uri = CanonicalKey::for_uri(&uri("file:///c:/proj/b.gd"));
        let from_path = CanonicalKey::for_path(camino::Utf8Path::new("C:/proj/b.gd")).unwrap();
        assert_eq!(from_uri, from_path);
        assert!(
            from_uri.as_str().starts_with("file:///C:/"),
            "drive must be folded to upper, got {}",
            from_uri.as_str()
        );
    }

    #[test]
    fn for_uri_reencodes_raw_subdelims_to_match_for_path() {
        // `lsp-types` 0.97 parses through `fluent-uri` 0.1.4, which does NOT
        // re-normalize percent-encoding, so a client may send a sub-delimiter RAW in the wire URI
        // (e.g. `file:///proj/Main(old).gd`). Pre-fix `for_uri` keyed those bytes verbatim while the
        // disk-walk reader (`for_path`) re-encodes them, splitting the cache and silently disabling
        // WP-R2 for any file whose name contains one of `( ) ! $ & ' + , ; =`. Unlike the existing
        // agreement tests, this builds the wire URI from a RAW string (not via `path_to_file_uri`,
        // which already encodes), reproducing exactly what a client can put on the wire. `for_uri`
        // now routes back through `for_path`, so writer and reader agree regardless of the client's
        // encoding. (`[ ] { }` are excluded: they aren't valid raw in a URI path, so a client can't
        // send them unencoded in the first place.)
        for c in ['(', ')', '!', '$', '&', '\'', '+', ',', ';', '='] {
            let path_str = format!("/proj/a{c}b.gd");
            let raw_wire: Uri = format!("file://{path_str}").parse().unwrap_or_else(|_| {
                panic!("a client may legally send a raw '{c}' in a file URI path")
            });
            let from_uri = CanonicalKey::for_uri(&raw_wire);
            let from_path = CanonicalKey::for_path(camino::Utf8Path::new(&path_str)).unwrap();
            assert_eq!(
                from_uri, from_path,
                "a raw '{c}' in the wire URI must canonicalize to the same key as the disk path"
            );
            assert!(
                !from_uri.as_str().contains(c),
                "the key must percent-encode a raw '{c}', got {}",
                from_uri.as_str()
            );
        }
    }

    #[test]
    fn uri_to_path_unc_preserves_host() {
        // RFC 8089 §E.3: `file://server/share/x` carries the host in the authority. uri_to_path must
        // reconstruct `//server/share/x`, not silently drop the host (which returned the wrong path
        // `/share/x` and resolved every nav request against a nonexistent file on a network share).
        let p = uri_to_path(&uri("file://fileserver/share/proj/a.gd")).unwrap();
        assert_eq!(p.as_str(), "//fileserver/share/proj/a.gd");
        // A space in a UNC path still decodes.
        let p2 = uri_to_path(&uri("file://fileserver/share/My%20Game/a.gd")).unwrap();
        assert_eq!(p2.as_str(), "//fileserver/share/My Game/a.gd");
        // `localhost` is the RFC alias for "this machine" — treated as a local (host-less) path.
        let local = uri_to_path(&uri("file://localhost/home/x/a.gd")).unwrap();
        assert_eq!(local.as_str(), "/home/x/a.gd");
    }

    #[test]
    fn uri_path_roundtrip_table() {
        // Lock `uri_to_path(path_to_file_uri(p)) == p` over a representative table
        // so a future lossy change to either direction fails in CI rather than in the field — the
        // UNC host-drop regression specifically.
        let mut cases: Vec<&str> = vec![
            "//fileserver/share/proj/enemy.gd",
            "//fileserver/share/My Game/h#2.gd",
        ];
        #[cfg(windows)]
        cases.extend(["C:/Users/Alice/My Game/hero.gd", "C:/proj/a.gd"]);
        #[cfg(not(windows))]
        cases.extend(["/home/alice/proj/a.gd", "/tmp/My Game/foo bar.gd"]);
        for case in cases {
            let p = camino::Utf8PathBuf::from(case);
            let u =
                path_to_file_uri(&p).unwrap_or_else(|| panic!("path_to_file_uri rejected {case}"));
            let back = uri_to_path(&u)
                .unwrap_or_else(|| panic!("uri_to_path rejected {} (from {case})", u.as_str()));
            assert_eq!(back.as_str(), case, "round-trip must preserve the path");
        }
    }

    #[test]
    fn path_to_file_uri_encodes_hash_and_question() {
        let p = camino::Utf8PathBuf::from("/tmp/has#hash/has?q.gd");
        let u = path_to_file_uri(&p).expect("hash/question paths should produce valid URIs");
        assert!(u.as_str().contains("%23"));
        assert!(u.as_str().contains("%3F"));
        let back = uri_to_path(&u).unwrap();
        assert_eq!(back.as_str(), "/tmp/has#hash/has?q.gd");
    }

    #[test]
    fn path_to_file_uri_leaves_safe_chars_unencoded() {
        let p = camino::Utf8PathBuf::from("/proj/src/foo-bar_baz.gd");
        let u = path_to_file_uri(&p).expect("ordinary path");
        assert_eq!(u.as_str(), "file:///proj/src/foo-bar_baz.gd");
    }

    #[cfg(windows)]
    #[test]
    fn path_to_file_uri_windows_drive_with_spaces() {
        let p = camino::Utf8PathBuf::from("C:/Users/Alice/My Game/hero.gd");
        let u = path_to_file_uri(&p).expect("space path on Windows");
        // Colon in the drive letter must NOT be encoded — it's path-safe.
        assert!(u.as_str().starts_with("file:///C:/"));
        assert!(u.as_str().contains("My%20Game"));
        let back = uri_to_path(&u).unwrap();
        assert_eq!(back.as_str(), "C:/Users/Alice/My Game/hero.gd");
    }

    #[cfg(windows)]
    #[test]
    fn windows_drive_letter() {
        let p = uri_to_path(&uri("file:///C:/Users/x/a.gd")).unwrap();
        assert_eq!(p.as_str(), "C:/Users/x/a.gd");
    }

    #[cfg(not(windows))]
    #[test]
    fn unix_absolute_path() {
        let p = uri_to_path(&uri("file:///home/x/a.gd")).unwrap();
        assert_eq!(p.as_str(), "/home/x/a.gd");
    }
}
