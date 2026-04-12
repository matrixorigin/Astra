//! Server-side caps for HTTP-driven list queries (`LIMIT` / `OFFSET`).

/// Aligns with other list endpoints (`skills`, `workflows`, `context` snapshots, etc.).
pub const MAX_API_LIST_LIMIT: u32 = 200;

/// Deep pagination is still expensive; cap `OFFSET` to bound worst-case skip work.
pub const MAX_API_LIST_OFFSET: u32 = 1_000_000;

/// Admin audit views may request more rows than standard user APIs.
pub const MAX_ADMIN_AUDIT_LOG_LIMIT: u32 = 500;

/// Ranked marketplace search already caps `limit`; bound `OFFSET` separately.
pub const MAX_MARKETPLACE_SEARCH_OFFSET: u32 = 50_000;

#[must_use]
pub fn clamp_api_list_pagination(limit: u32, offset: u32) -> (u32, u32) {
    (
        limit.min(MAX_API_LIST_LIMIT),
        offset.min(MAX_API_LIST_OFFSET),
    )
}

#[must_use]
pub fn clamp_admin_audit_limit(limit: u32) -> u32 {
    limit.min(MAX_ADMIN_AUDIT_LOG_LIMIT)
}

#[must_use]
pub fn clamp_marketplace_search_offset(offset: u32) -> u32 {
    offset.min(MAX_MARKETPLACE_SEARCH_OFFSET)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_api_list_pagination_caps_limit() {
        let (l, o) = clamp_api_list_pagination(u32::MAX, 0);
        assert_eq!(l, MAX_API_LIST_LIMIT);
        assert_eq!(o, 0);
    }

    #[test]
    fn clamp_api_list_pagination_caps_offset() {
        let (l, o) = clamp_api_list_pagination(10, u32::MAX);
        assert_eq!(l, 10);
        assert_eq!(o, MAX_API_LIST_OFFSET);
    }

    #[test]
    fn clamp_admin_audit_limit_caps() {
        assert_eq!(clamp_admin_audit_limit(100), 100);
        assert_eq!(clamp_admin_audit_limit(u32::MAX), MAX_ADMIN_AUDIT_LOG_LIMIT);
    }

    #[test]
    fn clamp_marketplace_search_offset_caps() {
        assert_eq!(clamp_marketplace_search_offset(0), 0);
        assert_eq!(
            clamp_marketplace_search_offset(u32::MAX),
            MAX_MARKETPLACE_SEARCH_OFFSET
        );
    }
}
