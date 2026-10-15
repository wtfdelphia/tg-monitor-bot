//! 控制面统一认证上下文。
//!
//! 入口层只负责把已验证的成员关系收敛成这里的值；业务层不接受裸 `tenant_id`。

use crate::ids::TenantId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSource {
    Web,
    Bot,
}

impl AuthSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Bot => "bot",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TenantRole {
    Owner,
    Admin,
    Member,
    Viewer,
    SuperAdmin,
}

impl TenantRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "TENANT_OWNER",
            Self::Admin => "TENANT_ADMIN",
            Self::Member => "MEMBER",
            Self::Viewer => "VIEWER",
            Self::SuperAdmin => "SUPER_ADMIN",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "TENANT_OWNER" => Some(Self::Owner),
            "TENANT_ADMIN" => Some(Self::Admin),
            "MEMBER" => Some(Self::Member),
            "VIEWER" => Some(Self::Viewer),
            "SUPER_ADMIN" => Some(Self::SuperAdmin),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthContext {
    tenant_id: TenantId,
    actor_user_id: i64,
    role: TenantRole,
    source: AuthSource,
    web_session_id: Option<[u8; 32]>,
}

impl AuthContext {
    pub const fn new(
        tenant_id: TenantId,
        actor_user_id: i64,
        role: TenantRole,
        source: AuthSource,
    ) -> Self {
        Self {
            tenant_id,
            actor_user_id,
            role,
            source,
            web_session_id: None,
        }
    }

    pub const fn with_web_session_id(mut self, session_id: [u8; 32]) -> Self {
        self.web_session_id = Some(session_id);
        self
    }

    pub const fn tenant_id(self) -> TenantId {
        self.tenant_id
    }

    pub const fn actor_user_id(self) -> i64 {
        self.actor_user_id
    }

    pub const fn role(self) -> TenantRole {
        self.role
    }

    pub const fn source(self) -> AuthSource {
        self.source
    }

    pub const fn web_session_id(self) -> Option<[u8; 32]> {
        self.web_session_id
    }
}

#[cfg(test)]
mod tests {
    use super::TenantRole;

    #[test]
    fn role_parser_is_closed_over_database_values() {
        assert_eq!(TenantRole::parse("TENANT_OWNER"), Some(TenantRole::Owner));
        assert_eq!(
            TenantRole::parse("SUPER_ADMIN"),
            Some(TenantRole::SuperAdmin)
        );
        assert_eq!(TenantRole::parse("owner"), None);
        assert_eq!(TenantRole::parse(""), None);
    }
}
