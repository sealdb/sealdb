/// F20: in-memory privilege Manager (TiDB `pkg/privilege/privilege.go`).
/// Roles: CREATE/DROP ROLE, GRANT/REVOKE role, SET ROLE, and active-role
/// privilege checks. Completeness wave: mysql.user persistence, dynamic
/// privileges, column grants, LDAP, auth plugins.
use sealdb_errno::{ERR_DBACCESS_DENIED_ERROR, ERR_TABLEACCESS_DENIED_ERROR, Error, Result};
use sealdb_parser::ast::{
    CreateRoleStmt, DropRoleStmt, GrantLevel, GrantLevelType, GrantRoleStmt, GrantStmt,
    PrivilegeType, RevokeRoleStmt, RevokeStmt, SetRoleKind, UserIdentity,
};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct GrantEntry {
    db: String,
    table: String,
    privs: PrivilegeType,
}

struct Store {
    /// User/role → privilege entries.
    grants: HashMap<(String, String), Vec<GrantEntry>>,
    /// Registered role names (`username`@`hostname`).
    roles: HashSet<(String, String)>,
    /// User → granted roles.
    role_graph: HashMap<(String, String), HashSet<(String, String)>>,
}

pub struct Manager {
    skip_grant_table: bool,
    inner: Mutex<Store>,
}

impl Manager {
    pub fn skip_with_grant() -> Self {
        Self {
            skip_grant_table: true,
            inner: Mutex::new(Store {
                grants: HashMap::new(),
                roles: HashSet::new(),
                role_graph: HashMap::new(),
            }),
        }
    }

    pub fn with_grant_table() -> Self {
        let mgr = Self {
            skip_grant_table: false,
            inner: Mutex::new(Store {
                grants: HashMap::new(),
                roles: HashSet::new(),
                role_graph: HashMap::new(),
            }),
        };
        mgr.grant_bits(
            &UserIdentity::root(),
            &GrantLevel {
                level: GrantLevelType::Global,
                db_name: String::new(),
                table_name: String::new(),
            },
            PrivilegeType::ALL,
        );
        mgr
    }

    pub fn skip_grant_table(&self) -> bool {
        self.skip_grant_table
    }

    pub fn grant(&self, stmt: &GrantStmt) -> Result<()> {
        let bits = merge_privs(&stmt.privs);
        for user in &stmt.users {
            self.grant_bits(user, &stmt.level, bits);
        }
        Ok(())
    }

    pub fn revoke(&self, stmt: &RevokeStmt) -> Result<()> {
        let bits = merge_privs(&stmt.privs);
        let mut inner = self.inner.lock().expect("privilege lock");
        for user in &stmt.users {
            let key = user_key(user);
            if let Some(entries) = inner.grants.get_mut(&key) {
                for entry in entries.iter_mut() {
                    if same_level(entry, &stmt.level) {
                        entry.privs = entry.privs.difference(bits);
                    }
                }
                entries.retain(|e| !e.privs.is_empty());
                if entries.is_empty() {
                    inner.grants.remove(&key);
                }
            }
        }
        Ok(())
    }

    pub fn create_role(&self, stmt: &CreateRoleStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for role in &stmt.roles {
            let key = user_key(role);
            if inner.roles.contains(&key) {
                if stmt.if_not_exists {
                    continue;
                }
                return Err(Error::new(
                    sealdb_errno::ERR_TABLE_EXISTS,
                    format!(
                        "Operation CREATE ROLE failed for '{}'@'{}'",
                        role.username, role.hostname
                    ),
                ));
            }
            inner.roles.insert(key);
        }
        Ok(())
    }

    pub fn drop_role(&self, stmt: &DropRoleStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for role in &stmt.roles {
            let key = user_key(role);
            if !inner.roles.remove(&key) {
                if stmt.if_exists {
                    continue;
                }
                return Err(Error::new(
                    sealdb_errno::ERR_NO_SUCH_TABLE,
                    format!(
                        "Operation DROP ROLE failed for '{}'@'{}'",
                        role.username, role.hostname
                    ),
                ));
            }
            inner.grants.remove(&key);
            for members in inner.role_graph.values_mut() {
                members.remove(&key);
            }
        }
        Ok(())
    }

    pub fn grant_role(&self, stmt: &GrantRoleStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for role in &stmt.roles {
            let rkey = user_key(role);
            if !inner.roles.contains(&rkey) {
                return Err(Error::new(
                    sealdb_errno::ERR_UNKNOWN_TABLE,
                    format!(
                        "Operation GRANT failed for '{}'@'{}'",
                        role.username, role.hostname
                    ),
                ));
            }
            for user in &stmt.users {
                inner
                    .role_graph
                    .entry(user_key(user))
                    .or_default()
                    .insert(rkey.clone());
            }
        }
        Ok(())
    }

    pub fn revoke_role(&self, stmt: &RevokeRoleStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for user in &stmt.users {
            let Some(members) = inner.role_graph.get_mut(&user_key(user)) else {
                continue;
            };
            for role in &stmt.roles {
                members.remove(&user_key(role));
            }
            if members.is_empty() {
                inner.role_graph.remove(&user_key(user));
            }
        }
        Ok(())
    }

    /// Roles granted to `user` (for SET ROLE ALL / SHOW GRANTS).
    pub fn granted_roles(&self, user: &UserIdentity) -> Vec<UserIdentity> {
        let inner = self.inner.lock().expect("privilege lock");
        let Some(members) = inner.role_graph.get(&user_key(user)) else {
            return Vec::new();
        };
        members
            .iter()
            .map(|(u, h)| UserIdentity::new(u.clone(), h.clone()))
            .collect()
    }

    /// Resolve active roles for SET ROLE.
    pub fn resolve_active_roles(
        &self,
        user: &UserIdentity,
        kind: &SetRoleKind,
    ) -> Result<Vec<UserIdentity>> {
        match kind {
            SetRoleKind::None => Ok(Vec::new()),
            SetRoleKind::All => Ok(self.granted_roles(user)),
            SetRoleKind::Roles(roles) => {
                let granted: HashSet<_> = self
                    .granted_roles(user)
                    .into_iter()
                    .map(|r| user_key(&r))
                    .collect();
                for role in roles {
                    if !granted.contains(&user_key(role)) {
                        return Err(Error::new(
                            sealdb_errno::ERR_UNKNOWN_TABLE,
                            "Access denied; you need (at least one of) the SUPER privilege(s) for this operation",
                        ));
                    }
                }
                Ok(roles.clone())
            }
        }
    }

    pub fn request_verification(
        &self,
        user: &UserIdentity,
        db: &str,
        table: &str,
        wanted: PrivilegeType,
    ) -> bool {
        self.request_verification_with_roles(user, &[], db, table, wanted)
    }

    /// Like `request_verification`, also accepting privileges from `active_roles`.
    pub fn request_verification_with_roles(
        &self,
        user: &UserIdentity,
        active_roles: &[UserIdentity],
        db: &str,
        table: &str,
        wanted: PrivilegeType,
    ) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        if has_priv(&inner, user, db, table, wanted) {
            return true;
        }
        active_roles
            .iter()
            .any(|role| has_priv(&inner, role, db, table, wanted))
    }

    pub fn show_grants(&self, user: &UserIdentity) -> Vec<String> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut out = Vec::new();
        if let Some(entries) = inner.grants.get(&user_key(user)) {
            out.extend(entries.iter().map(|e| format_grant(user, e)));
        }
        if let Some(roles) = inner.role_graph.get(&user_key(user)) {
            for (ru, rh) in roles {
                out.push(format!(
                    "GRANT '{}'@'{}' TO '{}'@'{}'",
                    ru, rh, user.username, user.hostname
                ));
            }
        }
        out
    }

    pub fn access_denied(
        db: &str,
        table: &str,
        wanted: PrivilegeType,
        user: &UserIdentity,
    ) -> Error {
        if table.is_empty() {
            Error::new(
                ERR_DBACCESS_DENIED_ERROR,
                format!(
                    "Access denied for user '{}'@'{}' to database '{}'",
                    user.username, user.hostname, db
                ),
            )
        } else {
            Error::new(
                ERR_TABLEACCESS_DENIED_ERROR,
                format!(
                    "{} command denied to user '{}'@'{}' for table '{}'",
                    wanted.names().join("/"),
                    user.username,
                    user.hostname,
                    table
                ),
            )
        }
    }

    fn grant_bits(&self, user: &UserIdentity, level: &GrantLevel, bits: PrivilegeType) {
        let mut inner = self.inner.lock().expect("privilege lock");
        let entries = inner.grants.entry(user_key(user)).or_default();
        if let Some(entry) = entries.iter_mut().find(|e| same_level(e, level)) {
            entry.privs = entry.privs.union(bits);
            return;
        }
        entries.push(GrantEntry {
            db: level.db_name.clone(),
            table: level.table_name.clone(),
            privs: bits,
        });
    }
}

fn has_priv(
    store: &Store,
    user: &UserIdentity,
    db: &str,
    table: &str,
    wanted: PrivilegeType,
) -> bool {
    let Some(entries) = store.grants.get(&user_key(user)) else {
        return false;
    };
    entries
        .iter()
        .any(|e| matches_scope(e, db, table) && e.privs.contains(wanted))
}

fn merge_privs(privs: &[sealdb_parser::ast::PrivElem]) -> PrivilegeType {
    privs
        .iter()
        .fold(PrivilegeType::default(), |acc, p| acc.union(p.tp))
}

fn user_key(user: &UserIdentity) -> (String, String) {
    (
        user.username.to_ascii_lowercase(),
        user.hostname.to_ascii_lowercase(),
    )
}

fn same_level(entry: &GrantEntry, level: &GrantLevel) -> bool {
    entry.db.eq_ignore_ascii_case(&level.db_name)
        && entry.table.eq_ignore_ascii_case(&level.table_name)
}

fn matches_scope(entry: &GrantEntry, db: &str, table: &str) -> bool {
    if entry.db.is_empty() && entry.table.is_empty() {
        return true;
    }
    if !entry.db.is_empty() && !entry.db.eq_ignore_ascii_case(db) {
        return false;
    }
    entry.table.is_empty() || entry.table.eq_ignore_ascii_case(table)
}

fn format_grant(user: &UserIdentity, entry: &GrantEntry) -> String {
    let level = if entry.db.is_empty() && entry.table.is_empty() {
        "*.*".to_string()
    } else if entry.table.is_empty() {
        format!("`{}`.*", entry.db)
    } else if entry.db.is_empty() {
        format!("`{}`", entry.table)
    } else {
        format!("`{}`.`{}`", entry.db, entry.table)
    };
    format!(
        "GRANT {} ON {} TO '{}'@'{}'",
        entry.privs.names().join(", "),
        level,
        user.username,
        user.hostname
    )
}
