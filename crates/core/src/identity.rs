/// Maximum persisted length for an Astra user principal.
///
/// Provider-authorized principals include the provider and external subject,
/// so they are intentionally wider than an internal UUID.
pub const USER_ID_MAX_LEN: usize = 128;

/// Maximum persisted length for a username or external subject display key.
pub const USERNAME_MAX_LEN: usize = USER_ID_MAX_LEN;
