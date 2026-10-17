//! Remote ref listing over the git wire protocol.

use std::{collections::HashMap, sync::Arc};

use actup_core::{Error, Result, error, version};
use bstr::{BString, ByteSlice};
use gix_features::progress::Discard;
use gix_protocol::{LsRefsCommand, handshake::Ref, ls_refs::RefPrefixes};
use gix_transport::{
    Protocol, Service,
    client::{Account, blocking_io::connect},
};
use tracing::debug;

use crate::{TagLike, process_tags};

const TAGS: &str = "refs/tags/";
const HEADS: &str = "refs/heads/";

/// A tag from a remote ref listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTag {
    name: String,
    sha: String,
}

impl RemoteTag {
    /// Create a tag from its short name and commit SHA.
    #[must_use]
    pub fn new(name: impl Into<String>, sha: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            sha: sha.into(),
        }
    }
}

impl TagLike for RemoteTag {
    fn name(&self) -> &str {
        &self.name
    }

    fn sha(&self) -> &str {
        &self.sha
    }
}

/// Every tag, branch and the default branch of a remote repository.
#[derive(Debug, Clone, Default)]
pub struct RemoteRefs {
    tags: Vec<RemoteTag>,
    branches: HashMap<String, String>,
    default_branch: Option<version::BranchInfo>,
}

impl RemoteRefs {
    /// Get the tags.
    #[must_use]
    pub fn tags(&self) -> &[RemoteTag] {
        &self.tags
    }

    /// Get the default branch, absent for an empty repository.
    #[must_use]
    pub const fn default_branch(&self) -> Option<&version::BranchInfo> {
        self.default_branch.as_ref()
    }

    /// Resolve a tag or branch name to its commit SHA, preferring tags.
    #[must_use]
    pub fn resolve(&self, ref_name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|t| t.name == ref_name)
            .map(|t| t.sha.as_str())
            .or_else(|| self.branches.get(ref_name).map(String::as_str))
    }

    /// Convert into a [`version::Set`] with branches and default branch attached.
    #[must_use]
    pub fn into_version_set(self) -> version::Set {
        let mut set = process_tags(self.tags);
        if let Some(branch) = self.default_branch {
            set.set_default_branch(branch);
        }
        set.set_branches(self.branches);
        set.set_listing(version::Listing::Complete);
        set
    }
}

/// List all tags and branches of a remote repository.
///
/// `token` authenticates as an HTTP basic-auth password, which is how GitHub,
/// GitLab and Gitea all accept tokens over the git smart-HTTP transport.
///
/// # Errors
///
/// Returns [`Error::Auth`] when the remote rejects the credentials,
/// [`Error::NotFound`] when the repository does not exist, and
/// [`Error::Network`] for any other transport failure.
pub async fn list_refs(url: String, token: Option<String>, repo: RepoLabel) -> Result<RemoteRefs> {
    debug!(url = %url, "listing refs over git protocol");

    tokio::task::spawn_blocking(move || list_refs_blocking(&url, token.as_deref(), &repo))
        .await
        .map_err(|e| Error::Internal {
            context: "ls-refs".into(),
            message: "ref listing task failed".into(),
            source: Some(Arc::new(e)),
        })?
}

/// Repository identity used to build structured errors.
#[derive(Debug, Clone)]
pub struct RepoLabel {
    owner: String,
    repo: String,
    host: Option<String>,
}

impl RepoLabel {
    /// Create a label from owner, repo and optional host.
    #[must_use]
    pub fn new(owner: impl Into<String>, repo: impl Into<String>, host: Option<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            host,
        }
    }
}

fn list_refs_blocking(url: &str, token: Option<&str>, repo: &RepoLabel) -> Result<RemoteRefs> {
    let mut transport = connect::connect(
        url,
        connect::Options {
            version: Protocol::V2,
            ..Default::default()
        },
    )
    .map_err(|e| transport_error(e, repo))?;

    if let Some(token) = token {
        transport
            .set_identity(Account {
                username: "x-access-token".into(),
                password: token.into(),
                oauth_refresh_token: None,
            })
            .map_err(|e| transport_error(e, repo))?;
    }

    let capabilities = transport
        .handshake(Service::UploadPack, &[])
        .map_err(|e| transport_error(e, repo))?
        .capabilities;

    let mut prefixes = RefPrefixes::new();
    prefixes.extend([TAGS, HEADS, "HEAD"].into_iter().map(BString::from));

    let refs = LsRefsCommand::new(
        Some(prefixes),
        &capabilities,
        ("agent", Some(gix_protocol::agent(agent_name()).into())),
    )
    .invoke_blocking(&mut transport, &mut Discard, false)
    .map_err(|e| transport_error(e, repo))?;

    Ok(collect(refs))
}

fn agent_name() -> String {
    format!("actup/{}", env!("CARGO_PKG_VERSION"))
}

fn collect(refs: Vec<Ref>) -> RemoteRefs {
    let mut out = RemoteRefs::default();
    let mut head_target: Option<String> = None;
    let mut head_sha: Option<String> = None;

    for r in refs {
        match r {
            // An annotated tag's ref points at the tag object; `object` is the commit it wraps.
            Ref::Peeled {
                full_ref_name,
                object,
                ..
            }
            | Ref::Direct {
                full_ref_name,
                object,
            } => {
                let name = full_ref_name.to_str_lossy();
                if let Some(tag) = name.strip_prefix(TAGS) {
                    out.tags.push(RemoteTag::new(tag, object.to_string()));
                } else if let Some(branch) = name.strip_prefix(HEADS) {
                    out.branches.insert(branch.to_string(), object.to_string());
                }
            }
            Ref::Symbolic {
                full_ref_name,
                target,
                object,
                ..
            } => {
                if full_ref_name == "HEAD" {
                    head_target = Some(target.to_str_lossy().into_owned());
                    head_sha = Some(object.to_string());
                }
            }
            Ref::Unborn {
                full_ref_name,
                target,
            } => {
                if full_ref_name == "HEAD" {
                    head_target = Some(target.to_str_lossy().into_owned());
                }
            }
        }
    }

    if let (Some(target), Some(sha)) = (head_target, head_sha) {
        let name = target.strip_prefix(HEADS).unwrap_or(&target).to_string();
        out.default_branch = Some(version::BranchInfo::new(name, sha));
    }

    out
}

fn transport_error<E>(source: E, repo: &RepoLabel) -> Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    match classify(&source) {
        Class::Auth => Error::Auth {
            provider: repo.host.clone().unwrap_or_else(|| "git".into()),
            reason: format!("{}/{}", repo.owner, repo.repo),
            source: Some(Arc::new(source)),
        },
        Class::NotFound => Error::NotFound {
            owner: repo.owner.clone(),
            repo: repo.repo.clone(),
            host: repo.host.clone(),
        },
        Class::Network => Error::Network {
            operation: format!("ls-refs {}/{}", repo.owner, repo.repo),
            message: String::new(),
            source: Some(Arc::new(source)),
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Class {
    Auth,
    NotFound,
    Network,
}

fn classify(source: &(dyn std::error::Error + 'static)) -> Class {
    let mut current = Some(source);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::PermissionDenied
        {
            return Class::Auth;
        }
        current = err.source();
    }

    classify_text(&error::chain(source))
}

fn classify_text(text: &str) -> Class {
    if text.contains("status 404") {
        Class::NotFound
    } else if text.contains("status 401") || text.contains("status 403") {
        Class::Auth
    } else {
        Class::Network
    }
}

#[cfg(test)]
mod tests {
    use gix_hash::ObjectId;

    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes_or_panic(&[byte; 20])
    }

    #[test]
    fn peeled_tag_yields_the_commit_not_the_tag_object() {
        let refs = vec![Ref::Peeled {
            full_ref_name: "refs/tags/v6.0.3".into(),
            tag: oid(0xaa),
            object: oid(0xbb),
        }];

        let collected = collect(refs);

        assert_eq!(collected.tags().len(), 1);
        assert_eq!(collected.tags()[0].name(), "v6.0.3");
        assert_eq!(collected.tags()[0].sha(), oid(0xbb).to_string());
    }

    #[test]
    fn head_symref_becomes_the_default_branch() -> TestResult {
        let refs = vec![
            Ref::Symbolic {
                full_ref_name: "HEAD".into(),
                target: "refs/heads/main".into(),
                tag: None,
                object: oid(0x11),
            },
            Ref::Direct {
                full_ref_name: "refs/heads/main".into(),
                object: oid(0x11),
            },
        ];

        let collected = collect(refs);
        let branch = collected
            .default_branch()
            .ok_or("`collected.default_branch()` is None")?;

        assert_eq!(branch.name(), "main");
        assert_eq!(branch.sha(), oid(0x11).to_string());
        assert_eq!(
            collected
                .resolve("main")
                .ok_or("`collected.resolve(..)` is None")?,
            oid(0x11).to_string()
        );
        Ok(())
    }

    #[test]
    fn resolve_prefers_a_tag_over_a_branch_of_the_same_name() -> TestResult {
        let refs = vec![
            Ref::Direct {
                full_ref_name: "refs/tags/v2".into(),
                object: oid(0x22),
            },
            Ref::Direct {
                full_ref_name: "refs/heads/v2".into(),
                object: oid(0x33),
            },
        ];

        let collected = collect(refs);

        assert_eq!(
            collected
                .resolve("v2")
                .ok_or("`collected.resolve(..)` is None")?,
            oid(0x22).to_string()
        );
        Ok(())
    }

    #[test]
    fn unknown_ref_does_not_resolve() {
        assert!(RemoteRefs::default().resolve("v9").is_none());
    }

    #[test]
    fn version_set_carries_every_tag_and_the_default_branch() -> TestResult {
        let refs = vec![
            Ref::Direct {
                full_ref_name: "refs/tags/v2".into(),
                object: oid(0x01),
            },
            Ref::Direct {
                full_ref_name: "refs/tags/v2.83.2".into(),
                object: oid(0x01),
            },
            Ref::Direct {
                full_ref_name: "refs/tags/cargo-hack".into(),
                object: oid(0x02),
            },
            Ref::Symbolic {
                full_ref_name: "HEAD".into(),
                target: "refs/heads/main".into(),
                tag: None,
                object: oid(0x03),
            },
        ];

        let set = collect(refs).into_version_set();

        assert!(set.has_exact_tag("v2"));
        assert_eq!(set.major_tags().len(), 1);
        assert_eq!(
            set.latest()
                .ok_or("`set.latest()` is None")?
                .version()
                .to_string(),
            "v2.83.2"
        );
        assert_eq!(
            set.default_branch()
                .ok_or("`set.default_branch()` is None")?
                .name(),
            "main"
        );
        assert_eq!(
            set.other_tag_sha("cargo-hack"),
            Some(oid(0x02).to_string().as_str())
        );
        assert_eq!(set.listing(), version::Listing::Complete);
        Ok(())
    }

    #[derive(Debug)]
    struct Handshake(std::io::Error);

    impl std::fmt::Display for Handshake {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("handshake failed")
        }
    }

    impl std::error::Error for Handshake {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    fn label() -> RepoLabel {
        RepoLabel::new("o", "r", Some("ghe.corp.com".into()))
    }

    #[test]
    fn transport_error_keeps_the_gix_error_as_source() {
        let err = transport_error(Handshake(std::io::Error::other("reset by peer")), &label());
        assert!(matches!(err, Error::Network { .. }), "{err:?}");
        assert_eq!(err.to_string(), "network error during ls-refs o/r");
        assert_eq!(
            error::chain(&err),
            "network error during ls-refs o/r: handshake failed: reset by peer"
        );

        let denied = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = transport_error(Handshake(denied), &label());
        assert!(matches!(err, Error::Auth { .. }), "{err:?}");
        assert_eq!(
            error::chain(&err),
            "authentication failed for ghe.corp.com: o/r: handshake failed: denied"
        );

        let missing = std::io::Error::other("Received HTTP status 404");
        let err = transport_error(Handshake(missing), &label());
        assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
    }

    #[test]
    fn http_status_classification() {
        assert_eq!(
            classify_text("Received HTTP status 404 Not Found"),
            Class::NotFound
        );
        assert_eq!(classify_text("Received HTTP status 401"), Class::Auth);
        assert_eq!(classify_text("connection refused"), Class::Network);
    }
}
