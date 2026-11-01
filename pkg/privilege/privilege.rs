/// F20 core: in-memory privilege Manager (TiDB `pkg/privilege/privilege.go`).
/// Completeness wave / not this feature: mysql.user persistence, roles,
/// dynamic privileges, column grants, LDAP, auth plugins.
use sealdb_errno::{ERR_DBACCESS_DENIED_ERROR, ERR_TABLEACCESS_DENIED_ERROR, Error, Result};
use sealdb_parser::ast::{
    GrantLevel, GrantLevelType, GrantStmt, PrivilegeType, RevokeStmt, UserIdentity,
};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug)]
struct GrantEntry {
    db: String,
    table: String,
    privs: PrivilegeType,
}

pub struct Manager {
    skip_grant_table: bool,
    inner: Mutex<HashMap<(String, String), Vec<GrantEntry>>>,
}

impl Manager {
    pub fn skip_with_grant() -> Self {
        Self {
            skip_grant_table: true,
            inner: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_grant_table() -> Self {
        let mgr = Self {
            skip_grant_table: false,
            inner: Mutex::new(HashMap::new()),
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
            if let Some(entries) = inner.get_mut(&key) {
                for entry in entries.iter_mut() {
                    if same_level(entry, &stmt.level) {
                        entry.privs = entry.privs.difference(bits);
                    }
                }
                entries.retain(|e| !e.privs.is_empty());
                if entries.is_empty() {
                    inner.remove(&key);
                }
            }
        }
        Ok(())
    }

    pub fn request_verification(
        &self,
        user: &UserIdentity,
        db: &str,
        table: &str,
        wanted: PrivilegeType,
    ) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        let Some(entries) = inner.get(&user_key(user)) else {
            return false;
        };
        entries
            .iter()
            .any(|e| matches_scope(e, db, table) && e.privs.contains(wanted))
    }

    pub fn show_grants(&self, user: &UserIdentity) -> Vec<String> {
        let inner = self.inner.lock().expect("privilege lock");
        let Some(entries) = inner.get(&user_key(user)) else {
            return Vec::new();
        };
        entries.iter().map(|e| format_grant(user, e)).collect()
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
        let entries = inner.entry(user_key(user)).or_default();
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
