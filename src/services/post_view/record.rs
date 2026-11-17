//! Recording post views.
//!
//! Every reader counts, logged in or not; what differs is how repeat views
//! are deduplicated, which [`Viewer`] decides:
//!
//! | viewer        | identity              | counted                     |
//! |---------------|-----------------------|-----------------------------|
//! | logged in     | user id               | once per post (echobackend) |
//! | anonymous     | IP + User-Agent       | once per post per 24h       |
//! | unidentified  | none (no IP resolved) | every request               |
//!
//! A Redis marker ([`ViewedMarker`]) short-circuits repeat views without a
//! DB lookup; the DB stays the source of truth when the marker is missing.
//! Automated clients ([`ClientInfo::is_likely_bot`]) are ignored silently.

use super::{PostViewError, ensure_post_viewable, has_user_viewed};
use crate::domain::client::ClientInfo;
use crate::entities::post_views;
use crate::infra::cache;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter, Set,
};
use sha2::{Digest, Sha256};
use std::time::Duration;
use uuid::Uuid;

/// How long an anonymous visitor's view blocks another one of the same post.
const ANONYMOUS_DEDUP_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// TTL of the "already viewed" Redis marker (echobackend's 24h). It only
/// saves DB lookups, so it must not outlive [`ANONYMOUS_DEDUP_WINDOW`].
const VIEWED_MARKER_TTL: Duration = ANONYMOUS_DEDUP_WINDOW;

/// Records a view of `post_id` by `user_id` (`None` when anonymous) from
/// `client`. Repeat views and automated clients are accepted but not stored.
pub async fn record_view(
    db: &DatabaseConnection,
    post_id: Uuid,
    user_id: Option<Uuid>,
    client: ClientInfo,
) -> Result<(), PostViewError> {
    if client.is_likely_bot() {
        return Ok(());
    }

    let viewer = Viewer::resolve(user_id, &client);
    let marker = viewer.marker_id().map(|id| ViewedMarker::new(post_id, id));
    if let Some(marker) = &marker
        && marker.is_set().await
    {
        return Ok(());
    }

    ensure_post_viewable(db, post_id).await?;

    if !viewer.has_viewed(db, post_id).await? {
        insert_view(db, post_id, viewer.user_id(), client).await?;
    }

    if let Some(marker) = &marker {
        marker.set().await;
    }
    Ok(())
}

/// Who is viewing a post, i.e. the identity repeat views are deduplicated on.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Viewer {
    User(Uuid),
    Anonymous {
        ip_address: String,
        user_agent: Option<String>,
    },
    /// Anonymous with no resolvable IP (the server runs without connect
    /// info, e.g. in tests): nothing to deduplicate on.
    Unidentified,
}

impl Viewer {
    fn resolve(user_id: Option<Uuid>, client: &ClientInfo) -> Self {
        match (user_id, &client.ip_address) {
            (Some(user_id), _) => Self::User(user_id),
            (None, Some(ip_address)) => Self::Anonymous {
                ip_address: ip_address.clone(),
                user_agent: client.user_agent.clone(),
            },
            (None, None) => Self::Unidentified,
        }
    }

    fn user_id(&self) -> Option<Uuid> {
        match self {
            Self::User(user_id) => Some(*user_id),
            Self::Anonymous { .. } | Self::Unidentified => None,
        }
    }

    /// Identity used in the Redis marker key. Anonymous visitors are hashed
    /// so raw IPs never end up in cache keys.
    fn marker_id(&self) -> Option<String> {
        match self {
            Self::User(user_id) => Some(user_id.to_string()),
            Self::Anonymous {
                ip_address,
                user_agent,
            } => {
                let mut hasher = Sha256::new();
                hasher.update(ip_address.as_bytes());
                hasher.update(b"|");
                hasher.update(user_agent.as_deref().unwrap_or_default().as_bytes());
                Some(format!("anon:{}", hex::encode(hasher.finalize())))
            }
            Self::Unidentified => None,
        }
    }

    /// Whether this viewer already has a stored view that counts against a
    /// new one.
    async fn has_viewed(&self, db: &DatabaseConnection, post_id: Uuid) -> Result<bool, DbErr> {
        match self {
            Self::User(user_id) => has_user_viewed(db, post_id, *user_id).await,
            Self::Anonymous {
                ip_address,
                user_agent,
            } => {
                let since = Utc::now()
                    - chrono::Duration::from_std(ANONYMOUS_DEDUP_WINDOW).expect("window fits");
                let query = post_views::Entity::find()
                    .filter(post_views::Column::PostId.eq(post_id))
                    .filter(post_views::Column::UserId.is_null())
                    .filter(post_views::Column::IpAddress.eq(ip_address.as_str()))
                    .filter(match user_agent {
                        Some(ua) => post_views::Column::UserAgent.eq(ua.as_str()),
                        None => post_views::Column::UserAgent.is_null(),
                    })
                    .filter(post_views::Column::CreatedAt.gt(since))
                    .filter(post_views::Column::DeletedAt.is_null());
                Ok(query.one(db).await?.is_some())
            }
            Self::Unidentified => Ok(false),
        }
    }
}

/// Redis marker `post_view:<post>:<viewer>` (JSON `true`) meaning "this
/// viewer's view of this post is already counted". Cache failures are
/// treated as a missing marker, so the DB check still runs.
struct ViewedMarker {
    post_id: String,
    viewer_id: String,
}

impl ViewedMarker {
    fn new(post_id: Uuid, viewer_id: String) -> Self {
        Self {
            post_id: post_id.to_string(),
            viewer_id,
        }
    }

    fn key(&self) -> [&str; 3] {
        ["post_view", &self.post_id, &self.viewer_id]
    }

    async fn is_set(&self) -> bool {
        cache::read_json::<bool>(&self.key()).await == Some(true)
    }

    async fn set(&self) {
        cache::write_json(&self.key(), &true, VIEWED_MARKER_TTL).await;
    }
}

async fn insert_view(
    db: &DatabaseConnection,
    post_id: Uuid,
    user_id: Option<Uuid>,
    client: ClientInfo,
) -> Result<(), DbErr> {
    let now = Utc::now();
    post_views::ActiveModel {
        id: Set(Uuid::now_v7()),
        post_id: Set(post_id),
        user_id: Set(user_id),
        ip_address: Set(client.ip_address),
        user_agent: Set(client.user_agent),
        created_at: Set(Some(now.into())),
        updated_at: Set(Some(now.into())),
        deleted_at: Set(None),
    }
    .insert(db)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(ip: Option<&str>, ua: Option<&str>) -> ClientInfo {
        ClientInfo {
            ip_address: ip.map(Into::into),
            user_agent: ua.map(Into::into),
        }
    }

    #[test]
    fn resolves_viewer_identity() {
        let user = Uuid::now_v7();
        let browser = client(Some("203.0.113.5"), Some("UA"));
        assert_eq!(Viewer::resolve(Some(user), &browser), Viewer::User(user));
        assert_eq!(
            Viewer::resolve(None, &browser),
            Viewer::Anonymous {
                ip_address: "203.0.113.5".into(),
                user_agent: Some("UA".into()),
            }
        );
        assert_eq!(
            Viewer::resolve(None, &client(None, Some("UA"))),
            Viewer::Unidentified
        );
    }

    #[test]
    fn anonymous_marker_is_hashed_stable_and_distinct() {
        let marker = |ip, ua| Viewer::resolve(None, &client(Some(ip), Some(ua))).marker_id();
        let a = marker("203.0.113.5", "UA").unwrap();
        assert_eq!(Some(a.clone()), marker("203.0.113.5", "UA"));
        assert_ne!(Some(a.clone()), marker("203.0.113.6", "UA"));
        assert_ne!(Some(a.clone()), marker("203.0.113.5", "UA2"));
        assert!(a.starts_with("anon:") && !a.contains("203.0.113.5"));
    }

    #[test]
    fn user_marker_is_the_user_id() {
        let user = Uuid::now_v7();
        assert_eq!(
            Viewer::User(user).marker_id(),
            Some(user.to_string()),
            "must stay compatible with echobackend's post_view:<post>:<user>"
        );
        assert_eq!(Viewer::Unidentified.marker_id(), None);
    }
}
