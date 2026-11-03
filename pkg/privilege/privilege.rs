/// in-memory privilege Manager (TiDB `pkg/privilege/privilege.go`).
/// Roles: CREATE/DROP ROLE, GRANT/REVOKE role, SET ROLE, and active-role
/// privilege checks. Grant state persists to Storage under `mprivilege`
/// (mysql.user / mysql.db / mysql.tables_priv equivalent for SealDB).
/// Completeness wave: dynamic privileges, column grants, LDAP, auth plugins.
use sealdb_errno::{ERR_DBACCESS_DENIED_ERROR, ERR_TABLEACCESS_DENIED_ERROR, Error, Result};
use sealdb_parser::ast::{
    CreateRoleStmt, CreateUserStmt, DropRoleStmt, DropUserStmt, GrantLevel, GrantLevelType,
    GrantRoleStmt, GrantStmt, PrivilegeType, RenameUserStmt, RevokeRoleStmt, RevokeStmt,
    SetPwdStmt, SetRoleKind, UserIdentity,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// Storage key for the privilege snapshot (alongside schema meta under `m`).
pub const PRIVILEGE_STORAGE_KEY: &[u8] = b"mprivilege";

#[derive(Clone, Debug)]
struct GrantEntry {
    db: String,
    table: String,
    privs: PrivilegeType,
    /// MySQL GRANT OPTION — required to GRANT the same privileges onward.
    with_grant: bool,
    /// Column grants (`GRANT SELECT (id) ON t`); empty means table-level.
    columns: Vec<String>,
}

struct Store {
    /// User/role → privilege entries.
    grants: HashMap<(String, String), Vec<GrantEntry>>,
    /// Registered role names (`username`@`hostname`).
    roles: HashSet<(String, String)>,
    /// User → granted roles.
    role_graph: HashMap<(String, String), HashSet<(String, String)>>,
    /// TiDB dynamic privileges (BACKUP_ADMIN, …) per user.
    dynamic: HashMap<(String, String), HashSet<String>>,
    /// Created users and optional auth string (IDENTIFIED BY).
    users: HashMap<(String, String), Option<String>>,
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
                dynamic: HashMap::new(),
                users: HashMap::new(),
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
                dynamic: HashMap::new(),
                users: HashMap::new(),
            }),
        };
        {
            let mut inner = mgr.inner.lock().expect("privilege lock");
            inner.users.insert(user_key(&UserIdentity::root()), None);
        }
        mgr.grant_bits(
            &UserIdentity::root(),
            &GrantLevel {
                level: GrantLevelType::Global,
                db_name: String::new(),
                table_name: String::new(),
            },
            PrivilegeType::ALL,
            true,
            &[],
        );
        mgr
    }

    pub fn skip_grant_table(&self) -> bool {
        self.skip_grant_table
    }

    pub fn grant(&self, stmt: &GrantStmt) -> Result<()> {
        if !stmt.dynamic_privs.is_empty() {
            let mut inner = self.inner.lock().expect("privilege lock");
            for user in &stmt.users {
                let set = inner.dynamic.entry(user_key(user)).or_default();
                for name in &stmt.dynamic_privs {
                    set.insert(name.to_ascii_uppercase());
                }
            }
            return Ok(());
        }
        for elem in &stmt.privs {
            for user in &stmt.users {
                self.grant_bits(user, &stmt.level, elem.tp, stmt.with_grant, &elem.cols);
            }
        }
        Ok(())
    }

    pub fn revoke(&self, stmt: &RevokeStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        if !stmt.dynamic_privs.is_empty() {
            for user in &stmt.users {
                let key = user_key(user);
                if let Some(set) = inner.dynamic.get_mut(&key) {
                    for name in &stmt.dynamic_privs {
                        set.remove(&name.to_ascii_uppercase());
                    }
                    if set.is_empty() {
                        inner.dynamic.remove(&key);
                    }
                }
            }
            return Ok(());
        }
        for user in &stmt.users {
            let key = user_key(user);
            if let Some(entries) = inner.grants.get_mut(&key) {
                for entry in entries.iter_mut() {
                    if !same_level(entry, &stmt.level) {
                        continue;
                    }
                    if stmt.grant_option {
                        entry.with_grant = false;
                        continue;
                    }
                    for elem in &stmt.privs {
                        if !entry.privs.contains(elem.tp) && !elem.tp.is_all() {
                            continue;
                        }
                        if elem.cols.is_empty() {
                            entry.privs = entry.privs.difference(elem.tp);
                            entry.columns.clear();
                        } else {
                            entry
                                .columns
                                .retain(|c| !elem.cols.iter().any(|x| x.eq_ignore_ascii_case(c)));
                            if entry.columns.is_empty() && !entry.privs.is_all() {
                                // Column-only revoke emptied the list — drop that priv bit.
                                entry.privs = entry.privs.difference(elem.tp);
                            }
                        }
                        if elem.tp.is_all() || entry.privs.is_empty() {
                            entry.with_grant = false;
                        }
                    }
                }
                entries.retain(|e| !e.privs.is_empty() || e.with_grant);
                if entries.is_empty() {
                    inner.grants.remove(&key);
                }
            }
        }
        Ok(())
    }

    pub fn create_user(&self, stmt: &CreateUserStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for spec in &stmt.users {
            let key = user_key(&spec.user);
            if inner.users.contains_key(&key) {
                if stmt.if_not_exists {
                    continue;
                }
                return Err(Error::new(
                    sealdb_errno::ERR_TABLE_EXISTS,
                    format!(
                        "Operation CREATE USER failed for '{}'@'{}'",
                        spec.user.username, spec.user.hostname
                    ),
                ));
            }
            inner.users.insert(key, spec.auth.clone());
        }
        Ok(())
    }

    pub fn drop_user(&self, stmt: &DropUserStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for user in &stmt.users {
            let key = user_key(user);
            if inner.users.remove(&key).is_none() {
                if stmt.if_exists {
                    continue;
                }
                return Err(Error::new(
                    sealdb_errno::ERR_NO_SUCH_TABLE,
                    format!(
                        "Operation DROP USER failed for '{}'@'{}'",
                        user.username, user.hostname
                    ),
                ));
            }
            inner.grants.remove(&key);
            inner.dynamic.remove(&key);
            inner.role_graph.remove(&key);
        }
        Ok(())
    }

    /// `RENAME USER old TO new` — move auth + grants under the new identity.
    pub fn rename_user(&self, stmt: &RenameUserStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for pair in &stmt.user_to_users {
            let old_key = user_key(&pair.old_user);
            let new_key = user_key(&pair.new_user);
            if old_key == new_key {
                continue;
            }
            if !inner.users.contains_key(&old_key) && !self.skip_grant_table {
                return Err(Error::new(
                    sealdb_errno::ERR_NO_SUCH_TABLE,
                    format!(
                        "Operation RENAME USER failed for '{}'@'{}'",
                        pair.old_user.username, pair.old_user.hostname
                    ),
                ));
            }
            if inner.users.contains_key(&new_key) {
                return Err(Error::new(
                    sealdb_errno::ERR_TABLE_EXISTS,
                    format!(
                        "Operation RENAME USER failed for '{}'@'{}'",
                        pair.new_user.username, pair.new_user.hostname
                    ),
                ));
            }
            let auth = inner.users.remove(&old_key).unwrap_or(None);
            inner.users.insert(new_key.clone(), auth);
            if let Some(grants) = inner.grants.remove(&old_key) {
                inner.grants.insert(new_key.clone(), grants);
            }
            if let Some(dyns) = inner.dynamic.remove(&old_key) {
                inner.dynamic.insert(new_key.clone(), dyns);
            }
            if let Some(roles) = inner.role_graph.remove(&old_key) {
                inner.role_graph.insert(new_key.clone(), roles);
            }
            // Role memberships pointing at the old user as a role name.
            if inner.roles.remove(&old_key) {
                inner.roles.insert(new_key.clone());
            }
            for members in inner.role_graph.values_mut() {
                if members.remove(&old_key) {
                    members.insert(new_key.clone());
                }
            }
        }
        Ok(())
    }

    /// `SET PASSWORD` — update auth string (mysql_native_password plugin, ).
    /// Stores the password the same way as `CREATE USER … IDENTIFIED BY`.
    pub fn set_password(&self, stmt: &SetPwdStmt, current: &UserIdentity) -> Result<()> {
        let target = stmt.user.clone().unwrap_or_else(|| current.clone());
        let key = user_key(&target);
        let mut inner = self.inner.lock().expect("privilege lock");
        if !inner.users.contains_key(&key) {
            // Ensure the user exists so SET PASSWORD can create the auth slot
            // (skip-grant and bootstrap root already present).
            if self.skip_grant_table {
                inner.users.insert(key.clone(), None);
            } else {
                return Err(Error::new(
                    sealdb_errno::ERR_NO_SUCH_TABLE,
                    format!(
                        "Operation SET PASSWORD failed for '{}'@'{}'",
                        target.username, target.hostname
                    ),
                ));
            }
        }
        let auth = encode_native_password(&stmt.password);
        inner.users.insert(key, Some(auth));
        Ok(())
    }

    /// Auth string for `user` (IDENTIFIED BY / SET PASSWORD).
    pub fn auth_string(&self, user: &UserIdentity) -> Option<String> {
        self.inner
            .lock()
            .expect("privilege lock")
            .users
            .get(&user_key(user))
            .cloned()
            .flatten()
    }

    pub fn user_exists(&self, user: &UserIdentity) -> bool {
        if self.skip_grant_table {
            return true;
        }
        self.inner
            .lock()
            .expect("privilege lock")
            .users
            .contains_key(&user_key(user))
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
        if let Some(dyns) = inner.dynamic.get(&user_key(user)) {
            for name in dyns {
                out.push(format!(
                    "GRANT {} ON *.* TO '{}'@'{}'",
                    name, user.username, user.hostname
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

    fn grant_bits(
        &self,
        user: &UserIdentity,
        level: &GrantLevel,
        bits: PrivilegeType,
        with_grant: bool,
        columns: &[String],
    ) {
        let mut inner = self.inner.lock().expect("privilege lock");
        let entries = inner.grants.entry(user_key(user)).or_default();
        if let Some(entry) = entries.iter_mut().find(|e| same_level(e, level)) {
            entry.privs = entry.privs.union(bits);
            entry.with_grant = entry.with_grant || with_grant;
            if columns.is_empty() {
                entry.columns.clear();
            } else if !entry.columns.is_empty() {
                for col in columns {
                    if !entry
                        .columns
                        .iter()
                        .any(|have| have.eq_ignore_ascii_case(col))
                    {
                        entry.columns.push(col.clone());
                    }
                }
            }
            return;
        }
        entries.push(GrantEntry {
            db: level.db_name.clone(),
            table: level.table_name.clone(),
            privs: bits,
            with_grant,
            columns: columns.to_vec(),
        });
    }

    /// Serialize grant tables for Storage (TiDB mysql.user / db / tables_priv).
    pub fn encode(&self) -> Result<Vec<u8>> {
        let inner = self.inner.lock().expect("privilege lock");
        let snap = PersistSnapshot {
            grants: inner
                .grants
                .iter()
                .map(|((u, h), entries)| PersistUserGrants {
                    user: u.clone(),
                    host: h.clone(),
                    entries: entries
                        .iter()
                        .map(|e| PersistGrantEntry {
                            db: e.db.clone(),
                            table: e.table.clone(),
                            privs: e.privs.bits(),
                            with_grant: e.with_grant,
                            columns: e.columns.clone(),
                        })
                        .collect(),
                })
                .collect(),
            roles: inner.roles.iter().cloned().collect(),
            role_graph: inner
                .role_graph
                .iter()
                .map(|((u, h), roles)| PersistRoleEdge {
                    user: u.clone(),
                    host: h.clone(),
                    roles: roles.iter().cloned().collect(),
                })
                .collect(),
            dynamic: inner
                .dynamic
                .iter()
                .map(|((u, h), names)| PersistDynamic {
                    user: u.clone(),
                    host: h.clone(),
                    names: names.iter().cloned().collect(),
                })
                .collect(),
            users: inner
                .users
                .iter()
                .map(|((u, h), auth)| PersistUser {
                    user: u.clone(),
                    host: h.clone(),
                    auth: auth.clone(),
                })
                .collect(),
        };
        serde_json::to_vec(&snap).map_err(|e| {
            Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                format!("encode privileges: {e}"),
            )
        })
    }

    /// Replace in-memory grant tables from a Storage snapshot.
    pub fn decode(&self, raw: &[u8]) -> Result<()> {
        if raw.is_empty() {
            return Ok(());
        }
        let snap: PersistSnapshot = serde_json::from_slice(raw).map_err(|e| {
            Error::new(
                sealdb_errno::ERR_UNKNOWN_ERROR,
                format!("decode privileges: {e}"),
            )
        })?;
        let mut grants = HashMap::new();
        for user in snap.grants {
            grants.insert(
                (user.user, user.host),
                user.entries
                    .into_iter()
                    .map(|e| GrantEntry {
                        db: e.db,
                        table: e.table,
                        privs: PrivilegeType::from_bits(e.privs),
                        with_grant: e.with_grant,
                        columns: e.columns,
                    })
                    .collect(),
            );
        }
        let roles = snap.roles.into_iter().collect();
        let mut role_graph = HashMap::new();
        for edge in snap.role_graph {
            role_graph.insert((edge.user, edge.host), edge.roles.into_iter().collect());
        }
        let mut dynamic = HashMap::new();
        for d in snap.dynamic {
            dynamic.insert((d.user, d.host), d.names.into_iter().collect());
        }
        let mut users = HashMap::new();
        for u in snap.users {
            users.insert((u.user, u.host), u.auth);
        }
        let mut inner = self.inner.lock().expect("privilege lock");
        inner.grants = grants;
        inner.roles = roles;
        inner.role_graph = role_graph;
        inner.dynamic = dynamic;
        if !users.is_empty() {
            inner.users = users;
        }
        Ok(())
    }

    /// True when `user` holds the named TiDB dynamic privilege.
    pub fn has_dynamic_priv(&self, user: &UserIdentity, name: &str) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        inner
            .dynamic
            .get(&user_key(user))
            .is_some_and(|s| s.contains(&name.to_ascii_uppercase()))
    }
}

#[derive(Serialize, Deserialize)]
struct PersistSnapshot {
    grants: Vec<PersistUserGrants>,
    roles: Vec<(String, String)>,
    role_graph: Vec<PersistRoleEdge>,
    #[serde(default)]
    dynamic: Vec<PersistDynamic>,
    #[serde(default)]
    users: Vec<PersistUser>,
}

#[derive(Serialize, Deserialize)]
struct PersistDynamic {
    user: String,
    host: String,
    names: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistUser {
    user: String,
    host: String,
    auth: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistUserGrants {
    user: String,
    host: String,
    entries: Vec<PersistGrantEntry>,
}

#[derive(Serialize, Deserialize)]
struct PersistGrantEntry {
    db: String,
    table: String,
    privs: u64,
    with_grant: bool,
    #[serde(default)]
    columns: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistRoleEdge {
    user: String,
    host: String,
    roles: Vec<(String, String)>,
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

fn user_key(user: &UserIdentity) -> (String, String) {
    (
        user.username.to_ascii_lowercase(),
        user.hostname.to_ascii_lowercase(),
    )
}

/// mysql_native_password-style auth string used by handshake.
/// Already-hashed values (`*…`) are kept; otherwise store the plaintext
/// (same shape as `CREATE USER … IDENTIFIED BY`, verified by plugin name).
fn encode_native_password(password: &str) -> String {
    if password.starts_with('*') {
        password.to_ascii_uppercase()
    } else {
        password.to_string()
    }
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
    let priv_names = if entry.columns.is_empty() {
        entry.privs.names().join(", ")
    } else {
        format!(
            "{} ({})",
            entry.privs.names().join(", "),
            entry.columns.join(", ")
        )
    };
    let mut s = format!(
        "GRANT {} ON {} TO '{}'@'{}'",
        priv_names, level, user.username, user.hostname
    );
    if entry.with_grant {
        s.push_str(" WITH GRANT OPTION");
    }
    s
}
