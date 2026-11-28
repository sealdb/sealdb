/// in-memory privilege Manager (TiDB `pkg/privilege/privilege.go`).
/// Roles: CREATE/DROP ROLE, GRANT/REVOKE role, SET ROLE, and active-role
/// privilege checks. Grant state persists to Storage under `mprivilege`
/// (mysql.user / mysql.db / mysql.tables_priv equivalent for SealDB).
/// Completeness wave: dynamic privileges, column grants, LDAP, auth plugins.
use sealdb_errno::{
    ERR_DBACCESS_DENIED_ERROR, ERR_DEFINER_TOO_LONG, ERR_TABLEACCESS_DENIED_ERROR, Error, Result,
};
use sealdb_parser::ast::{
    AlterUserStmt, CreateRoleStmt, CreateUserStmt, DropRoleStmt, DropUserStmt, GrantLevel,
    GrantLevelType, GrantRoleStmt, GrantStmt, PasswordExpireOption, PrivilegeType, RenameUserStmt,
    RequireClause, RevokeRoleStmt, RevokeStmt, SetDefaultRoleStmt, SetPwdStmt, SetRoleKind,
    UserIdentity,
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
    /// Union of `table_privs` and every column grant's privileges.
    privs: PrivilegeType,
    /// Privileges granted at table/db/global scope (not column-scoped).
    table_privs: PrivilegeType,
    /// MySQL GRANT OPTION — required to GRANT the same privileges onward.
    with_grant: bool,
    /// Column grants (`GRANT SELECT (id), UPDATE (b) ON t`): one entry per
    /// (privilege, column); empty means table-level.
    columns: Vec<(PrivilegeType, String)>,
}

/// One materialized `mysql.tables_priv` row (TiDB keeps the same grant
/// entries in memory and in the system table).
/// One materialized `mysql.columns_priv` row.
#[derive(Clone, Debug)]
pub struct ColumnPrivRow {
    pub host: String,
    pub db: String,
    pub user: String,
    pub table: String,
    pub column: String,
    pub column_priv: String,
}

/// One materialized `mysql.global_grants` row (dynamic privilege).
#[derive(Clone, Debug)]
pub struct GlobalGrantRow {
    pub user: String,
    pub host: String,
    pub privilege: String,
    pub with_grant: bool,
}

#[derive(Clone, Debug)]
pub struct TablePrivRow {
    pub host: String,
    pub db: String,
    pub user: String,
    pub table: String,
    pub grantor: String,
    pub table_priv: String,
    pub column_priv: String,
}

/// Global password-reuse policy (TiDB `password_history` /
/// `password_reuse_interval` sysvars).
#[derive(Clone, Copy, Debug, Default)]
pub struct PasswordPolicy {
    pub history: i64,
    pub reuse_days: i64,
}

/// One `mysql.password_history` row.
#[derive(Clone, Debug)]
pub struct PasswordHistoryRow {
    pub host: String,
    pub user: String,
    /// Unix epoch microseconds.
    pub timestamp_micros: i64,
    pub password: String,
}

/// Per-account password metadata kept in `mysql.user`.
#[derive(Clone, Debug)]
pub struct AccountPasswordOptions {
    /// `mysql.user.plugin` (empty renders as `mysql_native_password`).
    pub plugin: String,
    pub expired: bool,
    /// `mysql.user.Password_lifetime` (NULL = follow the global default).
    pub lifetime: Option<i64>,
    /// `mysql.user.Password_reuse_history`.
    pub reuse_history: Option<i64>,
    /// `mysql.user.Password_reuse_time` (days).
    pub reuse_time: Option<i64>,
}

impl Default for AccountPasswordOptions {
    fn default() -> Self {
        Self {
            plugin: "mysql_native_password".to_string(),
            expired: false,
            lifetime: None,
            reuse_history: None,
            reuse_time: None,
        }
    }
}

struct Store {
    /// User/role → privilege entries.
    grants: HashMap<(String, String), Vec<GrantEntry>>,
    /// Registered role names (`username`@`hostname`).
    roles: HashSet<(String, String)>,
    /// User → granted roles.
    role_graph: HashMap<(String, String), HashSet<(String, String)>>,
    /// TiDB dynamic privileges (BACKUP_ADMIN, …) per user; value is
    /// `WITH GRANT OPTION`.
    dynamic: HashMap<(String, String), HashMap<String, bool>>,
    /// Created users and optional auth string (IDENTIFIED BY).
    users: HashMap<(String, String), Option<String>>,
    /// TiDB `mysql.default_roles`: user → roles active by default at login.
    default_roles: HashMap<(String, String), HashSet<(String, String)>>,
    /// TiDB `mysql.user.User_attributes` payload (`COMMENT`, `RESOURCE GROUP`).
    attributes: HashMap<(String, String), UserAttributes>,
    /// `REQUIRE …` TLS requirements (`mysql.global_priv.Priv`).
    require: HashMap<(String, String), RequireClause>,
    /// `mysql.user` password lifetime / reuse / plugin metadata.
    password_options: HashMap<(String, String), AccountPasswordOptions>,
    /// `mysql.password_history` rows.
    password_history: Vec<PasswordHistoryRow>,
}

/// TiDB user attributes kept in `mysql.user.User_attributes` JSON.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UserAttributes {
    pub comment: Option<String>,
    pub resource_group: Option<String>,
}

impl UserAttributes {
    fn is_empty(&self) -> bool {
        self.comment.is_none() && self.resource_group.is_none()
    }

    /// TiDB's JSON text (`{"metadata": {"comment": "…"}, "resource_group": "…"}`).
    pub fn to_json(&self) -> String {
        let mut parts = Vec::new();
        if let Some(comment) = &self.comment {
            parts.push(format!(
                "\"metadata\": {{\"comment\": {}}}",
                json_string(comment)
            ));
        }
        if let Some(group) = &self.resource_group {
            parts.push(format!("\"resource_group\": {}", json_string(group)));
        }
        format!("{{{}}}", parts.join(", "))
    }
}

/// One materialized `mysql.user` row.
#[derive(Clone, Debug)]
pub struct UserRow {
    pub host: String,
    pub user: String,
    pub auth: Option<String>,
    pub attributes: String,
    pub global_privs: PrivilegeType,
    pub with_grant: bool,
    pub plugin: String,
    pub require: RequireClause,
    pub password_expired: bool,
    pub password_lifetime: Option<i64>,
    pub password_reuse_history: Option<i64>,
    pub password_reuse_time: Option<i64>,
}

/// One materialized `mysql.db` row (database-level grant).
#[derive(Clone, Debug)]
pub struct DbPrivRow {
    pub host: String,
    pub db: String,
    pub user: String,
    pub privs: PrivilegeType,
    pub with_grant: bool,
}

/// One materialized `mysql.global_priv` row (account JSON snapshot).
#[derive(Clone, Debug)]
pub struct GlobalPrivRow {
    pub host: String,
    pub user: String,
    pub priv_json: String,
}

/// One `information_schema.{SCHEMA,TABLE,COLUMN}_PRIVILEGES` row.
pub struct PrivRow {
    pub grantee: String,
    pub db: String,
    pub table: Option<String>,
    pub column: Option<String>,
    pub privilege: String,
    pub grantable: bool,
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
                default_roles: HashMap::new(),
                attributes: HashMap::new(),
                require: HashMap::new(),
                password_options: HashMap::new(),
                password_history: Vec::new(),
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
                default_roles: HashMap::new(),
                attributes: HashMap::new(),
                require: HashMap::new(),
                password_options: HashMap::new(),
                password_history: Vec::new(),
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
        check_dynamic_priv_level(&stmt.level, &stmt.dynamic_privs)?;
        // MySQL without `NO_AUTO_CREATE_USER` creates a missing account from
        // `GRANT … TO u` (with `IDENTIFIED BY` as its password). With the mode
        // on — TiDB's default — IMGRANT to a missing account fails with
        // ER_CANT_CREATE_USER_WITH_GRANT (1410). Roles are accounts too
        // (`CREATE ROLE`), so they are valid grantees.
        if !self.skip_grant_table {
            let mut inner = self.inner.lock().expect("privilege lock");
            let forbid_create = thread_no_auto_create_user();
            for user in &stmt.users {
                let key = user_key(user);
                if !inner.users.contains_key(&key) && !inner.roles.contains(&key) {
                    if forbid_create {
                        return Err(Error::new(
                            sealdb_errno::ERR_CANT_CREATE_USER_WITH_GRANT,
                            "You are not allowed to create a user with GRANT",
                        ));
                    }
                    inner.users.insert(key.clone(), None);
                }
            }
            for user in &stmt.users {
                let key = user_key(user);
                if let Some(auth) = &stmt.auth
                    && inner.users.contains_key(&key)
                {
                    inner
                        .users
                        .insert(key.clone(), Some(encode_native_password(auth)));
                }
                if let Some(require) = &stmt.require {
                    if require.none || require.is_empty() {
                        inner.require.remove(&key);
                    } else {
                        inner.require.insert(key.clone(), require.clone());
                    }
                }
            }
        }
        if !stmt.dynamic_privs.is_empty() {
            let mut inner = self.inner.lock().expect("privilege lock");
            for user in &stmt.users {
                for name in &stmt.dynamic_privs {
                    if !sealdb_parser::sqlparser_bridge::is_dynamic_priv(name) {
                        continue;
                    }
                    let set = inner.dynamic.entry(user_key(user)).or_default();
                    let slot = set.entry(name.to_ascii_uppercase()).or_insert(false);
                    *slot |= stmt.with_grant;
                }
            }
            if stmt.privs.is_empty() {
                return Ok(());
            }
        }
        for elem in &stmt.privs {
            for user in &stmt.users {
                self.grant_bits(user, &stmt.level, elem.tp, stmt.with_grant, &elem.cols);
            }
        }
        Ok(())
    }

    pub fn revoke(&self, stmt: &RevokeStmt) -> Result<()> {
        check_dynamic_priv_level(&stmt.level, &stmt.dynamic_privs)?;
        if !self.skip_grant_table {
            // TiDB rejects `REVOKE … FROM missing` before touching any grant.
            let inner = self.inner.lock().expect("privilege lock");
            for user in &stmt.users {
                let key = user_key(user);
                if !inner.users.contains_key(&key) && !inner.roles.contains(&key) {
                    return Err(Error::new(
                        sealdb_errno::ERR_UNKNOWN_ERROR,
                        format!("Unknown user: {}@{}", user.username, user.hostname),
                    ));
                }
            }
        }
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
            if stmt.privs.is_empty() {
                return Ok(());
            }
        }
        for user in &stmt.users {
            let key = user_key(user);
            if let Some(entries) = inner.grants.get_mut(&key) {
                for entry in entries.iter_mut() {
                    if !same_level(entry, &stmt.level) {
                        continue;
                    }
                    for elem in &stmt.privs {
                        if !entry.privs.contains(elem.tp) && !elem.tp.is_all() {
                            continue;
                        }
                        if elem.cols.is_empty() {
                            if elem.tp.is_all() {
                                entry.table_privs = PrivilegeType::default();
                                entry.columns.clear();
                            } else {
                                entry.table_privs = entry.table_privs.difference(elem.tp);
                            }
                        } else {
                            entry.columns.retain(|(granted, col)| {
                                !(granted.contains(elem.tp)
                                    && elem.cols.iter().any(|x| x.eq_ignore_ascii_case(col)))
                            });
                        }
                        // Rebuild the union from table-level and column grants.
                        entry.privs = entry
                            .columns
                            .iter()
                            .fold(entry.table_privs, |acc, (bits, _)| acc.union(*bits));
                        if elem.tp.is_all() || entry.privs.is_empty() {
                            entry.with_grant = false;
                        }
                    }
                    if stmt.grant_option {
                        entry.with_grant = false;
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

    /// Resolve the account a login from `client_host` authenticates as (TiDB
    /// `privilege/privileges` account matching): an exact host account wins,
    /// then `localhost` for loopback clients, then `%`, then any wildcard
    /// pattern. Unknown logins keep their client host so host-less users can
    /// still connect in tests.
    pub fn resolve_login(&self, username: &str, client_host: &str) -> UserIdentity {
        let client_host = if client_host.is_empty() {
            "localhost"
        } else {
            client_host
        };
        let inner = self.inner.lock().expect("privilege lock");
        let mut hosts: Vec<&str> = inner
            .users
            .keys()
            .filter(|(user, _)| user.eq_ignore_ascii_case(username))
            .map(|(_, host)| host.as_str())
            .collect();
        let loopback = matches!(
            client_host.to_ascii_lowercase().as_str(),
            "localhost" | "127.0.0.1" | "::1"
        );
        if let Some(host) = hosts
            .iter()
            .find(|host| host.eq_ignore_ascii_case(client_host))
            .copied()
        {
            return UserIdentity::new(username, host);
        }
        if loopback
            && let Some(host) = hosts
                .iter()
                .find(|host| host.eq_ignore_ascii_case("localhost"))
                .copied()
        {
            return UserIdentity::new(username, host);
        }
        if let Some(host) = hosts.iter().find(|host| **host == "%").copied() {
            return UserIdentity::new(username, host);
        }
        // Any remaining host is a `%`/`_` pattern: match it like `LIKE`.
        hosts.sort_unstable();
        if let Some(host) = hosts
            .iter()
            .find(|host| host_pattern_matches(host, client_host))
            .copied()
        {
            return UserIdentity::new(username, host);
        }
        UserIdentity::new(username, client_host)
    }

    pub fn create_user(&self, stmt: &CreateUserStmt, global: PasswordPolicy) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        let decl = PasswordDeclaration::from_specs(&stmt.users);
        // Validate every spec up front: TiDB runs the whole statement in one txn.
        for spec in &stmt.users {
            validate_account_name(&spec.user)?;
            validate_auth_plugin(spec)?;
            let key = user_key(&spec.user);
            if inner.users.contains_key(&key) && !stmt.if_not_exists {
                return Err(Error::new(
                    sealdb_errno::ERR_TABLE_EXISTS,
                    format!(
                        "Operation CREATE USER failed for '{}'@'{}'",
                        spec.user.username, spec.user.hostname
                    ),
                ));
            }
            if spec.user.username.is_empty() && decl.expire == Some(PasswordExpireOption::Expire) {
                return Err(anonymous_expire_error());
            }
            decl.validate()?;
        }
        for spec in &stmt.users {
            let key = user_key(&spec.user);
            if inner.users.contains_key(&key) {
                continue;
            }
            let plugin = spec
                .auth_plugin
                .clone()
                .unwrap_or_else(|| "mysql_native_password".to_string());
            let auth = spec
                .auth
                .as_deref()
                .map(|pwd| encode_password_for_plugin(&plugin, pwd, spec.auth_is_hash));
            inner.users.insert(key.clone(), auth.clone());
            if let Some(require) = &spec.require {
                if require.none || require.is_empty() {
                    inner.require.remove(&key);
                } else {
                    inner.require.insert(key.clone(), require.clone());
                }
            }
            let attrs = UserAttributes {
                comment: spec.comment.clone(),
                resource_group: spec.resource_group.clone(),
            };
            if !attrs.is_empty() {
                inner.attributes.insert(key.clone(), attrs);
            }
            let mut opts = AccountPasswordOptions {
                plugin: plugin.clone(),
                ..Default::default()
            };
            decl.apply_create(&mut opts)?;
            inner.password_options.insert(key.clone(), opts);
            if let Some(pwd) = auth
                && !pwd.is_empty()
            {
                let policy = reuse_info(&inner, &key, &decl, global);
                insert_history_if_enabled(&mut inner, &key, &pwd, &plugin, policy);
            }
        }
        Ok(())
    }

    /// `ALTER USER … IDENTIFIED … | PASSWORD … | COMMENT | RESOURCE GROUP`.
    /// TiDB applies every spec in one txn: a missing account fails the whole
    /// statement with 1396 unless `IF EXISTS` turns it into warning 3162.
    pub fn alter_user(
        &self,
        stmt: &AlterUserStmt,
        global: PasswordPolicy,
    ) -> Result<Vec<(u16, String)>> {
        let mut inner = self.inner.lock().expect("privilege lock");
        let decl = PasswordDeclaration::from_specs(&stmt.users);
        decl.validate()?;
        let mut missing = Vec::new();
        for spec in &stmt.users {
            validate_account_name(&spec.user)?;
            validate_auth_plugin(spec)?;
            let key = user_key(&spec.user);
            if !inner.users.contains_key(&key)
                && !inner.roles.contains(&key)
                && !self.skip_grant_table
            {
                missing.push((spec.user.clone(), key));
            }
        }
        let mut warnings = Vec::new();
        if !missing.is_empty() {
            if !stmt.if_exists {
                let user = &missing[0].0;
                return Err(Error::new(
                    sealdb_errno::ERR_CANNOT_USER,
                    format!(
                        "Operation ALTER USER failed for '{}'@'{}'",
                        user.username, user.hostname
                    ),
                ));
            }
            for (user, _) in &missing {
                warnings.push((
                    3162,
                    format!(
                        "User '{}'@'{}' does not exist.",
                        user.username, user.hostname
                    ),
                ));
            }
        }
        if decl.expire == Some(PasswordExpireOption::Expire) {
            for spec in &stmt.users {
                let key = user_key(&spec.user);
                if spec.user.username.is_empty()
                    && (inner.users.contains_key(&key) || inner.roles.contains(&key))
                {
                    return Err(anonymous_expire_error());
                }
            }
        }
        for spec in &stmt.users {
            let key = user_key(&spec.user);
            if !inner.users.contains_key(&key) {
                if inner.roles.contains(&key) || self.skip_grant_table {
                    inner.users.insert(key.clone(), None);
                } else {
                    continue;
                }
            }
            let current_plugin = inner
                .password_options
                .get(&key)
                .map(|opts| opts.plugin.clone())
                .unwrap_or_default();
            let current_plugin = normalize_plugin(&current_plugin).to_string();
            if spec.auth.is_some() || spec.auth_plugin.is_some() {
                let plugin = spec
                    .auth_plugin
                    .clone()
                    .unwrap_or_else(|| current_plugin.clone());
                // TiDB prunes the history when the auth plugin changes, even
                // when the statement does not supply a new password.
                if !plugin.eq_ignore_ascii_case(&current_plugin) {
                    inner
                        .password_history
                        .retain(|row| !(row.user == key.0 && row.host == key.1));
                }
                if spec.auth.is_some() {
                    let pwd = encode_password_for_plugin(
                        &plugin,
                        spec.auth.as_deref().unwrap_or(""),
                        spec.auth_is_hash,
                    );
                    if !pwd.is_empty() {
                        let policy = reuse_info(&inner, &key, &decl, global);
                        verify_and_record_password(&mut inner, &key, &pwd, &plugin, policy)?;
                    }
                    inner.users.insert(key.clone(), Some(pwd));
                }
                let opts = inner.password_options.entry(key.clone()).or_default();
                opts.plugin = plugin;
            }
            let opts = inner.password_options.entry(key.clone()).or_default();
            if spec.auth.is_some() && decl.expire.is_none() {
                opts.expired = false;
            }
            decl.apply_alter(opts);
            if let Some(require) = &spec.require {
                if require.none || require.is_empty() {
                    inner.require.remove(&key);
                } else {
                    inner.require.insert(key.clone(), require.clone());
                }
            }
            if spec.comment.is_some() || spec.resource_group.is_some() {
                let attrs = inner.attributes.entry(key).or_default();
                if let Some(comment) = &spec.comment {
                    attrs.comment = Some(comment.clone());
                }
                if let Some(group) = &spec.resource_group {
                    attrs.resource_group = Some(group.clone());
                }
            }
        }
        Ok(warnings)
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
            inner.attributes.remove(&key);
            inner.require.remove(&key);
            inner.password_options.remove(&key);
            inner
                .password_history
                .retain(|row| !(row.user == key.0 && row.host == key.1));
        }
        Ok(())
    }

    /// `RENAME USER old TO new` — move auth + grants under the new identity.
    /// Pairs are validated and applied in order (TiDB does the same inside one
    /// pessimistic txn), so a later pair can reuse a name vacated by an
    /// earlier one; the whole statement rolls back if any pair fails.
    pub fn rename_user(&self, stmt: &RenameUserStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        let failure = |pair: &sealdb_parser::ast::UserToUser, reason: &str| {
            Error::new(
                sealdb_errno::ERR_CANNOT_USER,
                format!(
                    "Operation RENAME USER failed for {}@{} TO {}@{} {reason}",
                    pair.old_user.username,
                    pair.old_user.hostname,
                    pair.new_user.username,
                    pair.new_user.hostname
                ),
            )
        };
        let snapshot = (
            inner.grants.clone(),
            inner.roles.clone(),
            inner.role_graph.clone(),
            inner.dynamic.clone(),
            inner.users.clone(),
            inner.default_roles.clone(),
            inner.attributes.clone(),
            inner.require.clone(),
            inner.password_options.clone(),
            inner.password_history.clone(),
        );
        let result = (|| -> Result<()> {
            for pair in &stmt.user_to_users {
                validate_account_name(&pair.new_user)?;
                if self.skip_grant_table {
                    continue;
                }
                let old_key = user_key(&pair.old_user);
                let new_key = user_key(&pair.new_user);
                if !inner.users.contains_key(&old_key) {
                    return Err(failure(pair, "old did not exist"));
                }
                if inner.users.contains_key(&new_key) {
                    return Err(failure(pair, "new did exist"));
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
                if let Some(attrs) = inner.attributes.remove(&old_key) {
                    inner.attributes.insert(new_key.clone(), attrs);
                }
                if let Some(require) = inner.require.remove(&old_key) {
                    inner.require.insert(new_key.clone(), require);
                }
                if let Some(defaults) = inner.default_roles.remove(&old_key) {
                    inner.default_roles.insert(new_key.clone(), defaults);
                }
                if let Some(opts) = inner.password_options.remove(&old_key) {
                    inner.password_options.insert(new_key.clone(), opts);
                }
                for row in inner.password_history.iter_mut() {
                    if row.user == old_key.0 && row.host == old_key.1 {
                        row.user = new_key.0.clone();
                        row.host = new_key.1.clone();
                    }
                }
                for roles in inner.default_roles.values_mut() {
                    if roles.remove(&old_key) {
                        roles.insert(new_key.clone());
                    }
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
        })();
        if result.is_err() {
            inner.grants = snapshot.0;
            inner.roles = snapshot.1;
            inner.role_graph = snapshot.2;
            inner.dynamic = snapshot.3;
            inner.users = snapshot.4;
            inner.default_roles = snapshot.5;
            inner.attributes = snapshot.6;
            inner.require = snapshot.7;
            inner.password_options = snapshot.8;
            inner.password_history = snapshot.9;
        }
        result
    }

    /// `SET DEFAULT ROLE {NONE|ALL|roles} TO users` (TiDB `mysql.default_roles`).
    pub fn set_default_role(&self, stmt: &SetDefaultRoleStmt) -> Result<()> {
        let mut inner = self.inner.lock().expect("privilege lock");
        for user in &stmt.users {
            let key = user_key(user);
            if !inner.users.contains_key(&key) {
                return Err(Error::new(
                    sealdb_errno::ERR_CANNOT_USER,
                    format!(
                        "Operation SET DEFAULT ROLE failed for {}@{}",
                        user.username, user.hostname
                    ),
                ));
            }
            let granted = inner.role_graph.get(&key).cloned().unwrap_or_default();
            let roles: HashSet<(String, String)> = match &stmt.kind {
                SetRoleKind::None => HashSet::new(),
                SetRoleKind::All => granted,
                SetRoleKind::Roles(roles) => {
                    let mut selected = HashSet::new();
                    for role in roles {
                        let role_key = user_key(role);
                        if !granted.contains(&role_key) {
                            return Err(Error::new(
                                sealdb_errno::ERR_ROLE_NOT_GRANTED,
                                format!(
                                    "`{}`@`{}` is not granted to {}@{}",
                                    role.username, role.hostname, user.username, user.hostname
                                ),
                            ));
                        }
                        selected.insert(role_key);
                    }
                    selected
                }
            };
            if roles.is_empty() {
                inner.default_roles.remove(&key);
            } else {
                inner.default_roles.insert(key, roles);
            }
        }
        Ok(())
    }

    /// Roles automatically active for `user` at login (`mysql.default_roles`).
    pub fn default_roles(&self, user: &UserIdentity) -> Vec<UserIdentity> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut roles: Vec<(String, String)> = inner
            .default_roles
            .get(&user_key(user))
            .map(|roles| roles.iter().cloned().collect())
            .unwrap_or_default();
        roles.sort();
        roles
            .into_iter()
            .map(|(u, h)| UserIdentity::new(u, h))
            .collect()
    }

    /// `SET PASSWORD` — update auth string, honouring the password-reuse policy
    /// (`mysql.password_history`) and clearing `password_expired`.
    pub fn set_password(
        &self,
        stmt: &SetPwdStmt,
        current: &UserIdentity,
        global: PasswordPolicy,
    ) -> Result<()> {
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
        let plugin = inner
            .password_options
            .get(&key)
            .map(|opts| opts.plugin.clone())
            .unwrap_or_default();
        let plugin = normalize_plugin(&plugin).to_string();
        let auth = encode_password_for_plugin(&plugin, &stmt.password, false);
        if !auth.is_empty() {
            let policy = reuse_info(&inner, &key, &PasswordDeclaration::default(), global);
            verify_and_record_password(&mut inner, &key, &auth, &plugin, policy)?;
        }
        inner.users.insert(key.clone(), Some(auth));
        let opts = inner.password_options.entry(key).or_default();
        opts.expired = false;
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
            inner.roles.insert(key.clone());
            inner.password_options.insert(
                key,
                AccountPasswordOptions {
                    expired: true,
                    ..Default::default()
                },
            );
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
            inner.password_options.remove(&key);
            inner
                .password_history
                .retain(|row| !(row.user == key.0 && row.host == key.1));
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
            // TiDB stores roles and users in the same `mysql.user` namespace:
            // a user can be granted to another account like a role.
            if !inner.roles.contains(&rkey) && !inner.users.contains_key(&rkey) {
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

    /// Whether `role` is directly granted to `user` (TiDB `FindEdge`). Empty
    /// role hosts default to `%`.
    pub fn role_granted_to(&self, user: &UserIdentity, role: &UserIdentity) -> bool {
        let inner = self.inner.lock().expect("privilege lock");
        let mut key = user_key(role);
        if key.1.is_empty() {
            key.1 = "%".to_string();
        }
        inner
            .role_graph
            .get(&user_key(user))
            .is_some_and(|members| members.contains(&key))
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

    /// TiDB `RequestDynamicVerification(..., requireGrantOption=false)`: a
    /// dynamic privilege held directly by the user or an active role, with the
    /// global `SUPER` (`ALL`) grant as fallback.
    pub fn request_dynamic_verification_with_roles(
        &self,
        user: &UserIdentity,
        active_roles: &[UserIdentity],
        name: &str,
    ) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        let holds = |identity: &UserIdentity| {
            if inner
                .dynamic
                .get(&user_key(identity))
                .is_some_and(|names| names.contains_key(name))
            {
                return true;
            }
            inner
                .grants
                .get(&user_key(identity))
                .is_some_and(|entries| {
                    entries.iter().any(|e| {
                        e.db.is_empty()
                            && e.table.is_empty()
                            && (e.table_privs.contains(PrivilegeType::ALL)
                                || e.table_privs.contains(PrivilegeType::SUPER))
                    })
                })
        };
        holds(user) || active_roles.iter().any(holds)
    }

    /// TiDB `AllPrivMask` request: any privilege granted at global, database,
    /// or table scope satisfies it (`SHOW CREATE TABLE`).
    pub fn request_any_table_verification_with_roles(
        &self,
        user: &UserIdentity,
        active_roles: &[UserIdentity],
        db: &str,
        table: &str,
    ) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        let has_any = |identity: &UserIdentity| {
            inner
                .grants
                .get(&user_key(identity))
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| matches_scope(e, db, table) && !e.privs.is_empty())
                })
        };
        has_any(user) || active_roles.iter().any(has_any)
    }

    /// `GRANT`/`REVOKE` need `GRANT OPTION` at the statement's level (or a
    /// broader scope). TiDB models this as `Grant_priv`, so a plain `ALL`
    /// grant without `WITH GRANT OPTION` does not satisfy it (error 8121).
    pub fn request_grant_option_with_roles(
        &self,
        user: &UserIdentity,
        active_roles: &[UserIdentity],
        db: &str,
        table: &str,
    ) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        let holds = |identity: &UserIdentity| {
            inner
                .grants
                .get(&user_key(identity))
                .is_some_and(|entries| {
                    entries
                        .iter()
                        .any(|e| matches_scope(e, db, table) && e.with_grant)
                })
        };
        holds(user) || active_roles.iter().any(holds)
    }

    /// Column-granular check (TiDB `RequestVerification` with a `column`): a
    /// global/db grant or a table-level grant covers every column, while a
    /// column grant covers only the listed columns.
    pub fn request_column_verification_with_roles(
        &self,
        user: &UserIdentity,
        active_roles: &[UserIdentity],
        db: &str,
        table: &str,
        column: &str,
        wanted: PrivilegeType,
    ) -> bool {
        if self.skip_grant_table {
            return true;
        }
        let inner = self.inner.lock().expect("privilege lock");
        if has_column_priv(&inner, user, db, table, column, wanted) {
            return true;
        }
        active_roles
            .iter()
            .any(|role| has_column_priv(&inner, role, db, table, column, wanted))
    }

    /// Databases visible in `SHOW DATABASES`: `None` when the user holds a
    /// global privilege (sees every database), otherwise the set of databases
    /// where the user has a db- or table-level grant (TiDB
    /// `ShowDatabasesExec` filters on `DBTablePrivilegeTable`).
    pub fn visible_databases(
        &self,
        user: &UserIdentity,
        active_roles: &[UserIdentity],
    ) -> Option<HashSet<String>> {
        if self.skip_grant_table {
            return None;
        }
        let inner = self.inner.lock().expect("privilege lock");
        let mut out = HashSet::new();
        for identity in std::iter::once(user).chain(active_roles.iter()) {
            let Some(entries) = inner.grants.get(&user_key(identity)) else {
                continue;
            };
            for entry in entries {
                if entry.db.is_empty() && entry.table.is_empty() {
                    return None;
                }
                if !entry.db.is_empty() {
                    out.insert(entry.db.to_ascii_lowercase());
                }
            }
        }
        Some(out)
    }

    /// TiDB `SHOW GRANTS`: global/db/table/column static grants (grouped and
    /// sorted per scope, `WITH GRANT OPTION` merged in), then role grants,
    /// then dynamic privileges. `USAGE` is printed when the displayed identity
    /// has no global static privilege (`hasGlobalGrant` convention).
    pub fn show_grants(&self, user: &UserIdentity, active_roles: &[UserIdentity]) -> Vec<String> {
        let inner = self.inner.lock().expect("privilege lock");
        // TiDB `FindAllUserEffectiveRoles`: active roles pull in the roles
        // granted to them, transitively.
        let mut effective_roles: Vec<UserIdentity> = Vec::new();
        let mut stack: Vec<UserIdentity> = active_roles.to_vec();
        while let Some(role) = stack.pop() {
            let key = user_key(&role);
            if effective_roles.iter().any(|r| user_key(r) == key) {
                continue;
            }
            if let Some(members) = inner.role_graph.get(&key) {
                stack.extend(
                    members
                        .iter()
                        .map(|(u, h)| UserIdentity::new(u.clone(), h.clone())),
                );
            }
            effective_roles.push(role);
        }
        let identities: Vec<&UserIdentity> = std::iter::once(user)
            .chain(effective_roles.iter())
            .collect();
        let mut gs: Vec<String> = Vec::new();
        let mut global_bits = PrivilegeType::default();
        let mut global_grant = false;
        let mut db_privs: std::collections::BTreeMap<String, (PrivilegeType, bool)> =
            std::collections::BTreeMap::new();
        let mut table_privs: std::collections::BTreeMap<String, (PrivilegeType, bool)> =
            std::collections::BTreeMap::new();
        let mut column_privs: std::collections::BTreeMap<String, Vec<(PrivilegeType, String)>> =
            std::collections::BTreeMap::new();
        for identity in &identities {
            let Some(entries) = inner.grants.get(&user_key(identity)) else {
                continue;
            };
            for entry in entries {
                if !entry.table_privs.is_empty() {
                    if entry.db.is_empty() && entry.table.is_empty() {
                        global_bits = global_bits.union(entry.table_privs);
                        global_grant |= entry.with_grant;
                    } else if entry.table.is_empty() {
                        let slot = db_privs
                            .entry(entry.db.to_ascii_lowercase())
                            .or_insert((PrivilegeType::default(), false));
                        slot.0 = slot.0.union(entry.table_privs);
                        slot.1 |= entry.with_grant;
                    } else {
                        let key = format!("`{}`.`{}`", entry.db, entry.table);
                        let slot = table_privs
                            .entry(key)
                            .or_insert((PrivilegeType::default(), false));
                        slot.0 = slot.0.union(entry.table_privs);
                        slot.1 |= entry.with_grant;
                    }
                }
                if !entry.columns.is_empty() {
                    let key = format!("`{}`.`{}`", entry.db, entry.table);
                    column_privs
                        .entry(key)
                        .or_default()
                        .extend(entry.columns.iter().cloned());
                }
            }
        }
        if global_bits.is_empty() {
            gs.push(format!(
                "GRANT USAGE ON *.* TO '{}'@'{}'",
                user.username, user.hostname
            ));
        } else {
            gs.push(static_grant_line(global_bits, "*.*", user, global_grant));
        }
        let db_start = gs.len();
        for (db, (bits, grant)) in &db_privs {
            let level = format!("`{db}`.*");
            if bits.is_empty() {
                if *grant {
                    gs.push(format!(
                        "GRANT USAGE ON {level} TO '{}'@'{}' WITH GRANT OPTION",
                        user.username, user.hostname
                    ));
                }
            } else {
                gs.push(static_grant_line(*bits, &level, user, *grant));
            }
        }
        // TiDB sorts each privilege scope's rendered lines (not the db names).
        gs[db_start..].sort();
        let db_end = gs.len();
        for (table, (bits, grant)) in &table_privs {
            if bits.is_empty() {
                if *grant {
                    gs.push(format!(
                        "GRANT USAGE ON {table} TO '{}'@'{}' WITH GRANT OPTION",
                        user.username, user.hostname
                    ));
                }
            } else {
                gs.push(static_grant_line(*bits, table, user, *grant));
            }
        }
        gs[db_end..].sort();
        let col_start = gs.len();
        for (table, entries) in &column_privs {
            let mut parts: Vec<String> = Vec::new();
            // Render in TiDB's privilege display order, merging columns.
            for (name, bit) in PrivilegeType::DISPLAY_ORDER {
                let mut cols: Vec<String> = Vec::new();
                for (granted, col) in entries {
                    if !granted.contains(bit) {
                        continue;
                    }
                    if !cols.iter().any(|c| c.eq_ignore_ascii_case(col)) {
                        cols.push(col.clone());
                    }
                }
                if cols.is_empty() {
                    continue;
                }
                parts.push(format!(
                    "{}({})",
                    name,
                    cols.iter()
                        .map(|c| format!("`{c}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if !parts.is_empty() {
                gs.push(format!(
                    "GRANT {} ON {} TO '{}'@'{}'",
                    parts.join(", "),
                    table,
                    user.username,
                    user.hostname
                ));
            }
        }
        gs[col_start..].sort();
        // Role grants of the displayed user (not inherited role privileges).
        if let Some(roles) = inner.role_graph.get(&user_key(user)) {
            let mut names: Vec<String> =
                roles.iter().map(|(u, h)| format!("'{u}'@'{h}'")).collect();
            names.sort();
            if !names.is_empty() {
                gs.push(format!(
                    "GRANT {} TO '{}'@'{}'",
                    names.join(", "),
                    user.username,
                    user.hostname
                ));
            }
        }
        // Dynamic privileges: sorted names, grouped by grant-option status.
        let mut dynamic_plain: Vec<String> = Vec::new();
        let mut dynamic_grant: Vec<String> = Vec::new();
        let mut seen: std::collections::BTreeMap<String, bool> = std::collections::BTreeMap::new();
        for identity in &identities {
            let Some(names) = inner.dynamic.get(&user_key(identity)) else {
                continue;
            };
            for (name, grantable) in names {
                let slot = seen.entry(name.clone()).or_insert(false);
                *slot = *slot || *grantable;
            }
        }
        for (name, grantable) in seen {
            if grantable {
                dynamic_grant.push(name);
            } else {
                dynamic_plain.push(name);
            }
        }
        for (names, grantable) in [(dynamic_plain, false), (dynamic_grant, true)] {
            if names.is_empty() {
                continue;
            }
            let mut s = format!(
                "GRANT {} ON *.* TO '{}'@'{}'",
                names.join(","),
                user.username,
                user.hostname
            );
            if grantable {
                s.push_str(" WITH GRANT OPTION");
            }
            gs.push(s);
        }
        gs
    }

    pub fn access_denied(
        db: &str,
        table: &str,
        wanted: PrivilegeType,
        user: &UserIdentity,
    ) -> Error {
        Self::access_denied_command(db, table, &wanted.names().join("/"), user)
    }

    /// Same as [`Self::access_denied`] with an explicit command label: TiDB
    /// reports `SHOW`/`LOCK STATS` privilege failures under the statement's
    /// own command name rather than the checked privilege.
    pub fn access_denied_command(
        db: &str,
        table: &str,
        command: &str,
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
                    command, user.username, user.hostname, table
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
                // A table-level grant subsumes column grants for these bits.
                entry.table_privs = entry.table_privs.union(bits);
                entry
                    .columns
                    .retain(|(granted, _)| !bits.contains(*granted));
            } else {
                for col in columns {
                    if !entry.columns.iter().any(|(granted, have)| {
                        granted.bits() == bits.bits() && have.eq_ignore_ascii_case(col)
                    }) {
                        entry.columns.push((bits, col.clone()));
                    }
                }
            }
            return;
        }
        entries.push(GrantEntry {
            db: level.db_name.clone(),
            table: level.table_name.clone(),
            privs: bits,
            table_privs: if columns.is_empty() {
                bits
            } else {
                PrivilegeType::default()
            },
            with_grant,
            columns: columns.iter().map(|col| (bits, col.clone())).collect(),
        });
    }

    /// Materialize `mysql.tables_priv` rows from the in-memory grants.
    /// Only table-scope entries produce rows (TiDB writes one row per
    /// `GRANT … ON db.table`, with column grants in `column_priv`).
    pub fn table_priv_rows(&self) -> Vec<TablePrivRow> {
        let inner = self.inner.lock().expect("privilege lock");
        let grantor = thread_user()
            .map(|user| user.display())
            .unwrap_or_else(|| "root@%".to_string());
        let mut rows = Vec::new();
        for ((user, host), entries) in &inner.grants {
            for entry in entries {
                if entry.db.is_empty() || entry.table.is_empty() {
                    continue;
                }
                let table_priv = entry.table_privs.table_priv_set_names().join(",");
                let column_priv = entry
                    .columns
                    .iter()
                    .fold(PrivilegeType::default(), |acc, (bits, _)| acc.union(*bits))
                    .table_priv_set_names()
                    .into_iter()
                    .filter(|name| matches!(*name, "Select" | "Insert" | "Update"))
                    .collect::<Vec<_>>()
                    .join(",");
                rows.push(TablePrivRow {
                    host: host.clone(),
                    db: entry.db.clone(),
                    user: user.clone(),
                    table: entry.table.clone(),
                    grantor: grantor.clone(),
                    table_priv,
                    column_priv,
                });
            }
        }
        rows.sort_by(|a, b| {
            (&a.host, &a.db, &a.user, &a.table).cmp(&(&b.host, &b.db, &b.user, &b.table))
        });
        rows
    }

    /// Materialize `mysql.columns_priv` rows: one row per column grant.
    pub fn column_priv_rows(&self) -> Vec<ColumnPrivRow> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut rows = Vec::new();
        for ((user, host), entries) in &inner.grants {
            for entry in entries {
                if entry.db.is_empty() || entry.table.is_empty() {
                    continue;
                }
                for (bits, column) in &entry.columns {
                    let name = bits
                        .table_priv_set_names()
                        .into_iter()
                        .filter(|name| {
                            matches!(*name, "Select" | "Insert" | "Update" | "References")
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    if name.is_empty() {
                        continue;
                    }
                    rows.push(ColumnPrivRow {
                        host: host.clone(),
                        db: entry.db.clone(),
                        user: user.clone(),
                        table: entry.table.clone(),
                        column: column.clone(),
                        column_priv: name,
                    });
                }
            }
        }
        rows.sort_by(|a, b| {
            (&a.host, &a.db, &a.user, &a.table, &a.column)
                .cmp(&(&b.host, &b.db, &b.user, &b.table, &b.column))
        });
        rows
    }

    /// Materialize `mysql.global_grants` rows: dynamic privileges per account.
    pub fn global_grant_rows(&self) -> Vec<GlobalGrantRow> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut rows = Vec::new();
        for ((user, host), set) in &inner.dynamic {
            for (name, with_grant) in set {
                rows.push(GlobalGrantRow {
                    user: user.clone(),
                    host: host.clone(),
                    privilege: name.clone(),
                    with_grant: *with_grant,
                });
            }
        }
        rows.sort_by(|a, b| {
            (&a.user, &a.host, &a.privilege).cmp(&(&b.user, &b.host, &b.privilege))
        });
        rows
    }

    /// Rows for `information_schema.{SCHEMA,TABLE,COLUMN}_PRIVILEGES`.
    pub fn privilege_rows(&self) -> Vec<PrivRow> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut rows = Vec::new();
        for ((user, host), entries) in &inner.grants {
            let grantee = format!("'{user}'@'{host}'");
            for entry in entries {
                if entry.db.is_empty() {
                    continue;
                }
                for name in entry.table_privs.names() {
                    rows.push(PrivRow {
                        grantee: grantee.clone(),
                        db: entry.db.clone(),
                        table: (!entry.table.is_empty()).then(|| entry.table.clone()),
                        column: None,
                        privilege: name.to_string(),
                        grantable: entry.with_grant,
                    });
                }
                for (bits, column) in &entry.columns {
                    for name in bits.names() {
                        rows.push(PrivRow {
                            grantee: grantee.clone(),
                            db: entry.db.clone(),
                            table: (!entry.table.is_empty()).then(|| entry.table.clone()),
                            column: Some(column.clone()),
                            privilege: name.to_string(),
                            grantable: entry.with_grant,
                        });
                    }
                }
            }
        }
        rows.sort_by(|a, b| {
            (&a.grantee, &a.db, &a.table, &a.column, &a.privilege).cmp(&(
                &b.grantee,
                &b.db,
                &b.table,
                &b.column,
                &b.privilege,
            ))
        });
        rows
    }

    /// Whether `user` holds any global grant (sees every privilege row).
    pub fn has_global_grant(&self, user: &UserIdentity) -> bool {
        let inner = self.inner.lock().expect("privilege lock");
        inner.grants.get(&user_key(user)).is_some_and(|entries| {
            entries
                .iter()
                .any(|e| e.db.is_empty() && !e.table_privs.is_empty())
        })
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
                            columns: e
                                .columns
                                .iter()
                                .map(|(bits, column)| PersistColumnGrant::Grant {
                                    privs: bits.bits(),
                                    column: column.clone(),
                                })
                                .collect(),
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
                    names: names.keys().cloned().collect(),
                    grant_names: names
                        .iter()
                        .filter(|(_, grantable)| **grantable)
                        .map(|(name, _)| name.clone())
                        .collect(),
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
            default_roles: inner
                .default_roles
                .iter()
                .map(|((u, h), roles)| PersistDefaultRoles {
                    user: u.clone(),
                    host: h.clone(),
                    roles: roles.iter().cloned().collect(),
                })
                .collect(),
            attributes: inner
                .attributes
                .iter()
                .map(|((u, h), attrs)| PersistAttributes {
                    user: u.clone(),
                    host: h.clone(),
                    comment: attrs.comment.clone(),
                    resource_group: attrs.resource_group.clone(),
                })
                .collect(),
            require: inner
                .require
                .iter()
                .map(|((u, h), require)| PersistRequire {
                    user: u.clone(),
                    host: h.clone(),
                    none: require.none,
                    ssl: require.ssl,
                    x509: require.x509,
                    issuer: require.issuer.clone(),
                    subject: require.subject.clone(),
                    cipher: require.cipher.clone(),
                })
                .collect(),
            password_options: inner
                .password_options
                .iter()
                .map(|((u, h), opts)| PersistPasswordOptions {
                    user: u.clone(),
                    host: h.clone(),
                    plugin: opts.plugin.clone(),
                    expired: opts.expired,
                    lifetime: opts.lifetime,
                    reuse_history: opts.reuse_history,
                    reuse_time: opts.reuse_time,
                })
                .collect(),
            password_history: inner
                .password_history
                .iter()
                .map(|row| PersistPasswordHistory {
                    host: row.host.clone(),
                    user: row.user.clone(),
                    timestamp_micros: row.timestamp_micros,
                    password: row.password.clone(),
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
                    .map(|e| {
                        // `ALL` is not a separate bit: snapshots from before
                        // SHOW VIEW (0x3f), CREATE VIEW (0x7f) or the full
                        // MySQL privilege set (0xff) was tracked stored exactly
                        // those bits as root's global ALL. Treat the old masks
                        // as today's ALL.
                        let privs = PrivilegeType::from_bits(e.privs);
                        let privs = if matches!(privs.bits(), 0x3f | 0x7f | 0xff) {
                            PrivilegeType::ALL
                        } else {
                            privs
                        };
                        let column_grants: Vec<(PrivilegeType, String)> = e
                            .columns
                            .into_iter()
                            .map(|column| match column {
                                PersistColumnGrant::Legacy(name) => (privs, name),
                                PersistColumnGrant::Grant { privs, column } => {
                                    (PrivilegeType::from_bits(privs), column)
                                }
                            })
                            .collect();
                        let column_bits = column_grants
                            .iter()
                            .fold(PrivilegeType::default(), |acc, (bits, _)| acc.union(*bits));
                        let table_privs = if column_grants.is_empty() {
                            privs
                        } else {
                            privs.difference(column_bits)
                        };
                        GrantEntry {
                            db: e.db,
                            table: e.table,
                            privs,
                            table_privs,
                            with_grant: e.with_grant,
                            columns: column_grants,
                        }
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
            let names = d
                .names
                .into_iter()
                .map(|name| {
                    let grantable = d.grant_names.iter().any(|g| g == &name);
                    (name, grantable)
                })
                .collect();
            dynamic.insert((d.user, d.host), names);
        }
        let mut users = HashMap::new();
        for u in snap.users {
            users.insert((u.user, u.host), u.auth);
        }
        let mut attributes = HashMap::new();
        for a in snap.attributes {
            attributes.insert(
                (a.user, a.host),
                UserAttributes {
                    comment: a.comment,
                    resource_group: a.resource_group,
                },
            );
        }
        let mut default_roles = HashMap::new();
        for edge in snap.default_roles {
            default_roles.insert((edge.user, edge.host), edge.roles.into_iter().collect());
        }
        let mut require = HashMap::new();
        for r in snap.require {
            require.insert(
                (r.user, r.host),
                RequireClause {
                    none: r.none,
                    ssl: r.ssl,
                    x509: r.x509,
                    issuer: r.issuer,
                    subject: r.subject,
                    cipher: r.cipher,
                },
            );
        }
        let mut inner = self.inner.lock().expect("privilege lock");
        inner.grants = grants;
        inner.roles = roles;
        inner.role_graph = role_graph;
        inner.dynamic = dynamic;
        inner.attributes = attributes;
        inner.default_roles = default_roles;
        inner.require = require;
        let mut password_options = HashMap::new();
        for opts in snap.password_options {
            password_options.insert(
                (opts.user, opts.host),
                AccountPasswordOptions {
                    plugin: opts.plugin,
                    expired: opts.expired,
                    lifetime: opts.lifetime,
                    reuse_history: opts.reuse_history,
                    reuse_time: opts.reuse_time,
                },
            );
        }
        inner.password_options = password_options;
        inner.password_history = snap
            .password_history
            .into_iter()
            .map(|row| PasswordHistoryRow {
                host: row.host,
                user: row.user,
                timestamp_micros: row.timestamp_micros,
                password: row.password,
            })
            .collect();
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
            .is_some_and(|s| s.contains_key(&name.to_ascii_uppercase()))
    }

    /// TiDB `mysql.user.User_attributes` for `user` (empty when never set).
    pub fn user_attributes(&self, user: &UserIdentity) -> UserAttributes {
        self.inner
            .lock()
            .expect("privilege lock")
            .attributes
            .get(&user_key(user))
            .cloned()
            .unwrap_or_default()
    }

    /// Rows for the virtual `mysql.user` table: one per created account, with
    /// the global static privileges and `User_attributes` JSON.
    pub fn user_rows(&self) -> Vec<UserRow> {
        let inner = self.inner.lock().expect("privilege lock");
        // TiDB materializes roles in `mysql.user` too (they are accounts).
        let mut accounts: Vec<((String, String), Option<String>)> = inner
            .users
            .iter()
            .map(|(key, auth)| (key.clone(), auth.clone()))
            .collect();
        for role in &inner.roles {
            if !inner.users.contains_key(role) {
                accounts.push((role.clone(), None));
            }
        }
        accounts.sort_by(|((u1, h1), _), ((u2, h2), _)| h1.cmp(h2).then(u1.cmp(u2)));
        accounts
            .into_iter()
            .map(|((user, host), auth)| {
                let mut privs = PrivilegeType::default();
                let mut with_grant = false;
                for entry in inner
                    .grants
                    .get(&(user.clone(), host.clone()))
                    .into_iter()
                    .flatten()
                {
                    if entry.db.is_empty() && entry.table.is_empty() {
                        privs = privs.union(entry.table_privs);
                        with_grant |= entry.with_grant;
                    }
                }
                let attributes = inner
                    .attributes
                    .get(&(user.clone(), host.clone()))
                    .map(UserAttributes::to_json)
                    .unwrap_or_else(|| "{}".to_string());
                let opts = inner
                    .password_options
                    .get(&(user.clone(), host.clone()))
                    .cloned()
                    .unwrap_or_default();
                UserRow {
                    host: host.clone(),
                    user: user.clone(),
                    auth: auth.clone(),
                    attributes,
                    global_privs: privs,
                    with_grant,
                    plugin: normalize_plugin(&opts.plugin).to_string(),
                    require: inner
                        .require
                        .get(&(user.clone(), host.clone()))
                        .cloned()
                        .unwrap_or_default(),
                    password_expired: opts.expired,
                    password_lifetime: opts.lifetime,
                    password_reuse_history: opts.reuse_history,
                    password_reuse_time: opts.reuse_time,
                }
            })
            .collect()
    }

    /// `REQUIRE …` TLS requirement for `user` (empty when not set).
    pub fn require_of(&self, user: &UserIdentity) -> RequireClause {
        self.inner
            .lock()
            .expect("privilege lock")
            .require
            .get(&user_key(user))
            .cloned()
            .unwrap_or_default()
    }

    /// Whether the account (user or role) exists.
    pub fn account_exists(&self, user: &UserIdentity) -> bool {
        let inner = self.inner.lock().expect("privilege lock");
        let key = user_key(user);
        inner.users.contains_key(&key) || inner.roles.contains(&key)
    }

    /// Rows for the virtual `mysql.password_history` table.
    pub fn password_history_rows(&self) -> Vec<PasswordHistoryRow> {
        let mut rows = self
            .inner
            .lock()
            .expect("privilege lock")
            .password_history
            .clone();
        rows.sort_by(|a, b| {
            (&a.user, &a.host, a.timestamp_micros).cmp(&(&b.user, &b.host, b.timestamp_micros))
        });
        rows
    }

    /// Move one `mysql.password_history` row's timestamp (`UPDATE … SET
    /// Password_timestamp = …` targets the `mysql.password_history` virtual
    /// table).
    pub fn set_password_history_timestamp(
        &self,
        user: &str,
        host: &str,
        old_micros: i64,
        new_micros: i64,
    ) {
        let mut inner = self.inner.lock().expect("privilege lock");
        for row in inner.password_history.iter_mut() {
            if row.user == user
                && row.host.eq_ignore_ascii_case(host)
                && row.timestamp_micros == old_micros
            {
                row.timestamp_micros = new_micros;
                break;
            }
        }
    }

    /// `SHOW CREATE USER` DDL for `user` (TiDB `ShowCreateUserExec`).
    pub fn show_create_user(&self, user: &UserIdentity) -> Option<String> {
        let inner = self.inner.lock().expect("privilege lock");
        let key = user_key(user);
        let auth = inner.users.get(&key)?;
        let require = inner.require.get(&key).cloned().unwrap_or_default();
        Some(format!(
            "CREATE USER '{}'@'{}' IDENTIFIED WITH 'mysql_native_password' AS '{}' {} PASSWORD EXPIRE DEFAULT ACCOUNT UNLOCK PASSWORD HISTORY DEFAULT PASSWORD REUSE INTERVAL DEFAULT",
            user.username,
            user.hostname,
            auth.as_deref().unwrap_or(""),
            require.restore()
        ))
    }

    /// Rows for the virtual `mysql.db` table: one per database-level grant.
    pub fn db_priv_rows(&self) -> Vec<DbPrivRow> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut rows = Vec::new();
        for ((user, host), entries) in &inner.grants {
            for entry in entries {
                if entry.db.is_empty() || !entry.table.is_empty() {
                    continue;
                }
                rows.push(DbPrivRow {
                    host: host.clone(),
                    db: entry.db.clone(),
                    user: user.clone(),
                    privs: entry.table_privs,
                    with_grant: entry.with_grant,
                });
            }
        }
        rows.sort_by(|a, b| (&a.host, &a.db, &a.user).cmp(&(&b.host, &b.db, &b.user)));
        rows
    }

    /// Rows for the virtual `mysql.global_priv` table: the account JSON
    /// snapshot (SSL fields only; empty privileges render as `{}`).
    pub fn global_priv_rows(&self) -> Vec<GlobalPrivRow> {
        let inner = self.inner.lock().expect("privilege lock");
        let mut accounts: Vec<((String, String), ())> = inner
            .users
            .keys()
            .chain(inner.roles.iter())
            .map(|key| (key.clone(), ()))
            .collect();
        accounts.sort_by(|((u1, h1), _), ((u2, h2), _)| h1.cmp(h2).then(u1.cmp(u2)));
        accounts
            .into_iter()
            .map(|((user, host), _)| GlobalPrivRow {
                priv_json: inner
                    .require
                    .get(&(user.clone(), host.clone()))
                    .map(RequireClause::priv_json)
                    .unwrap_or_else(|| "{}".to_string()),
                host,
                user,
            })
            .collect()
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
    /// `mysql.default_roles` (roles active at login).
    #[serde(default)]
    default_roles: Vec<PersistDefaultRoles>,
    /// User_attributes (`COMMENT` / `RESOURCE GROUP`).
    #[serde(default)]
    attributes: Vec<PersistAttributes>,
    /// `REQUIRE …` TLS requirements (`mysql.global_priv.Priv`).
    #[serde(default)]
    require: Vec<PersistRequire>,
    /// `mysql.user` password metadata.
    #[serde(default)]
    password_options: Vec<PersistPasswordOptions>,
    /// `mysql.password_history` rows.
    #[serde(default)]
    password_history: Vec<PersistPasswordHistory>,
}

#[derive(Serialize, Deserialize)]
struct PersistPasswordOptions {
    user: String,
    host: String,
    #[serde(default)]
    plugin: String,
    #[serde(default)]
    expired: bool,
    #[serde(default)]
    lifetime: Option<i64>,
    #[serde(default)]
    reuse_history: Option<i64>,
    #[serde(default)]
    reuse_time: Option<i64>,
}

#[derive(Serialize, Deserialize)]
struct PersistPasswordHistory {
    host: String,
    user: String,
    timestamp_micros: i64,
    password: String,
}

#[derive(Serialize, Deserialize)]
struct PersistRequire {
    user: String,
    host: String,
    #[serde(default)]
    none: bool,
    #[serde(default)]
    ssl: bool,
    #[serde(default)]
    x509: bool,
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    cipher: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistDefaultRoles {
    user: String,
    host: String,
    roles: Vec<(String, String)>,
}

#[derive(Serialize, Deserialize)]
struct PersistAttributes {
    user: String,
    host: String,
    #[serde(default)]
    comment: Option<String>,
    #[serde(default)]
    resource_group: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PersistDynamic {
    user: String,
    host: String,
    names: Vec<String>,
    /// Dynamic privileges granted `WITH GRANT OPTION`.
    #[serde(default)]
    grant_names: Vec<String>,
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
    columns: Vec<PersistColumnGrant>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(untagged)]
enum PersistColumnGrant {
    Legacy(String),
    Grant { privs: u64, column: String },
}

#[derive(Serialize, Deserialize)]
struct PersistRoleEdge {
    user: String,
    host: String,
    roles: Vec<(String, String)>,
}

/// TiDB `ErrIllegalPrivilegeLevel` (3619): dynamic privileges (`BACKUP_ADMIN`,
/// `SYSTEM_VARIABLES_ADMIN`, …) only exist at the global level, even when they
/// appear in a mixed list with table privileges.
fn check_dynamic_priv_level(level: &GrantLevel, dynamic_privs: &[String]) -> Result<()> {
    let registered: Vec<&String> = dynamic_privs
        .iter()
        .filter(|name| sealdb_parser::sqlparser_bridge::is_dynamic_priv(name))
        .collect();
    if registered.is_empty() || level.level == GrantLevelType::Global {
        return Ok(());
    }
    Err(Error::new(
        sealdb_errno::ERR_ILLEGAL_PRIVILEGE_LEVEL,
        format!(
            "Illegal privilege level specified for {}",
            registered
                .iter()
                .map(|name| name.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ),
    ))
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
    // A column-only grant does not satisfy a table-level request (TiDB keeps
    // `mysql.tables_priv` and `mysql.columns_priv` apart). A multi-bit request
    // passes when any requested privilege is held (`priv & wanted != 0`).
    entries
        .iter()
        .any(|e| matches_scope(e, db, table) && e.table_privs.intersects(wanted))
}

/// Column-level request: table grants cover every column, column grants only
/// the columns they name (TiDB `MySQLPrivilege.RequestVerification`).
fn has_column_priv(
    store: &Store,
    user: &UserIdentity,
    db: &str,
    table: &str,
    column: &str,
    wanted: PrivilegeType,
) -> bool {
    let Some(entries) = store.grants.get(&user_key(user)) else {
        return false;
    };
    entries.iter().any(|e| {
        matches_scope(e, db, table)
            && (e.table_privs.intersects(wanted)
                || e.columns.iter().any(|(granted, have)| {
                    granted.intersects(wanted) && have.eq_ignore_ascii_case(column)
                }))
    })
}

/// TiDB `ErrWrongStringLength`: account names above MySQL's limits (32-char
/// user, 255-char host) are rejected with 1470.
fn validate_account_name(user: &UserIdentity) -> Result<()> {
    if user.username.chars().count() > 32 {
        return Err(Error::new(
            ERR_DEFINER_TOO_LONG,
            format!(
                "String '{}' is too long for user name (should be no longer than 32)",
                user.username.chars().take(70).collect::<String>()
            ),
        ));
    }
    if user.hostname.chars().count() > 255 {
        return Err(Error::new(
            ERR_DEFINER_TOO_LONG,
            format!(
                "String '{}' is too long for host name (should be no longer than 255)",
                user.hostname.chars().take(70).collect::<String>()
            ),
        ));
    }
    Ok(())
}

fn user_key(user: &UserIdentity) -> (String, String) {
    // MySQL user names are case-sensitive (they are printed verbatim by
    // `mysql.user`/`mysql.db`), while hosts are not.
    (user.username.clone(), user.hostname.to_ascii_lowercase())
}

/// MySQL host-pattern match (`%` any run, `_` one char).
fn host_pattern_matches(pattern: &str, host: &str) -> bool {
    fn go(pattern: &[u8], host: &[u8]) -> bool {
        match pattern.split_first() {
            None => host.is_empty(),
            Some((b'%', rest)) => (0..=host.len()).any(|i| go(rest, &host[i..])),
            Some((b'_', rest)) => !host.is_empty() && go(rest, &host[1..]),
            Some((c, rest)) => {
                host.first().is_some_and(|h| h.eq_ignore_ascii_case(c)) && go(rest, &host[1..])
            }
        }
    }
    go(pattern.as_bytes(), host.as_bytes())
}

/// mysql_native_password-style auth string used by handshake.
/// Already-hashed values (`*…`) are kept; otherwise store the plaintext
/// (same shape as `CREATE USER … IDENTIFIED BY`, verified by plugin name).
/// TiDB's default plugin name when the account has no explicit one.
fn normalize_plugin(plugin: &str) -> &str {
    if plugin.is_empty() {
        "mysql_native_password"
    } else {
        plugin
    }
}

/// Auth plugins TiDB ships (unknown names fail with 1524).
const KNOWN_AUTH_PLUGINS: &[&str] = &[
    "mysql_native_password",
    "caching_sha2_password",
    "tidb_sm3_password",
    "auth_socket",
    "tidb_auth_token",
    "authentication_ldap_simple",
    "authentication_ldap_sasl",
];

fn validate_auth_plugin(spec: &sealdb_parser::ast::UserSpec) -> Result<()> {
    if let Some(plugin) = &spec.auth_plugin
        && !KNOWN_AUTH_PLUGINS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(plugin))
    {
        return Err(Error::new(1524, format!("Plugin '{plugin}' is not loaded")));
    }
    Ok(())
}

fn anonymous_expire_error() -> Error {
    Error::new(3016, "The password for anonymous user cannot be expired.")
}

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn hex_upper(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// Encode `pwd` the way TiDB stores it for `plugin`. SealDB uses deterministic
/// hashes for the salted TiDB plugins: only equality (the reuse policy) is
/// observed by the SQL surface.
fn encode_password_for_plugin(plugin: &str, pwd: &str, is_hash: bool) -> String {
    if is_hash {
        return pwd.to_string();
    }
    match plugin.to_ascii_lowercase().as_str() {
        "auth_socket" => String::new(),
        "caching_sha2_password" => format!(
            "$A$005${}",
            hex_upper(&sha1_digest(format!("caching_sha2:{pwd}").as_bytes()))
        ),
        "tidb_sm3_password" => format!(
            "*{}",
            hex_upper(&sha1_digest(format!("sm3:{pwd}").as_bytes()))
        ),
        _ => encode_native_password(pwd),
    }
}

/// Statement-level `PASSWORD …` options of `CREATE USER` / `ALTER USER`
/// (they trail the user list and apply to every account in the statement).
#[derive(Clone, Default)]
struct PasswordDeclaration {
    expire: Option<PasswordExpireOption>,
    history: Option<Option<u64>>,
    reuse: Option<Option<u64>>,
}

impl PasswordDeclaration {
    fn from_specs(specs: &[sealdb_parser::ast::UserSpec]) -> Self {
        let mut decl = Self::default();
        for spec in specs {
            if spec.password_expire.is_some() {
                decl.expire = spec.password_expire.clone();
            }
            if spec.password_history.is_some() {
                decl.history = spec.password_history;
            }
            if spec.password_reuse.is_some() {
                decl.reuse = spec.password_reuse;
            }
        }
        decl
    }

    fn validate(&self) -> Result<()> {
        if let Some(PasswordExpireOption::Interval(days)) = &self.expire
            && (*days == 0 || *days > u16::MAX as u64)
        {
            return Err(Error::new(
                1690,
                format!("Incorrect value: '{days}' for DAY"),
            ));
        }
        Ok(())
    }

    fn apply_create(&self, opts: &mut AccountPasswordOptions) -> Result<()> {
        self.apply_alter(opts);
        Ok(())
    }

    fn apply_alter(&self, opts: &mut AccountPasswordOptions) {
        match &self.expire {
            Some(PasswordExpireOption::Expire) => opts.expired = true,
            Some(PasswordExpireOption::Default) => opts.lifetime = None,
            Some(PasswordExpireOption::Never) => opts.lifetime = Some(0),
            Some(PasswordExpireOption::Interval(days)) => opts.lifetime = Some(*days as i64),
            None => {}
        }
        if let Some(history) = self.history {
            opts.reuse_history = history.map(|value| value.min(u16::MAX as u64) as i64);
        }
        if let Some(reuse) = self.reuse {
            opts.reuse_time = reuse.map(|value| value.min(u16::MAX as u64) as i64);
        }
    }
}

/// Effective reuse policy for one password change: account values fall back to
/// the globals, and a statement-level `PASSWORD …` clause overrides both.
fn reuse_info(
    inner: &Store,
    key: &(String, String),
    decl: &PasswordDeclaration,
    global: PasswordPolicy,
) -> PasswordPolicy {
    let opts = inner.password_options.get(key);
    let mut policy = PasswordPolicy {
        history: opts
            .and_then(|opts| opts.reuse_history)
            .unwrap_or(global.history),
        reuse_days: opts
            .and_then(|opts| opts.reuse_time)
            .unwrap_or(global.reuse_days),
    };
    if let Some(history) = decl.history {
        policy.history = history
            .map(|value| value.min(u16::MAX as u64) as i64)
            .unwrap_or(global.history);
    }
    if let Some(reuse) = decl.reuse {
        policy.reuse_days = reuse
            .map(|value| value.min(u16::MAX as u64) as i64)
            .unwrap_or(global.reuse_days);
    }
    policy
}

/// Whether the account's policy asks TiDB to retain password history.
fn history_enabled(policy: PasswordPolicy) -> bool {
    policy.history > 0 || policy.reuse_days > 0
}

fn push_history(inner: &mut Store, key: &(String, String), pwd: &str) {
    inner.password_history.push(PasswordHistoryRow {
        host: key.1.clone(),
        user: key.0.clone(),
        timestamp_micros: now_micros(),
        password: pwd.to_string(),
    });
}

/// `CREATE USER` records the initial password when the policy is enabled (no
/// reuse check: the account has no history yet).
fn insert_history_if_enabled(
    inner: &mut Store,
    key: &(String, String),
    pwd: &str,
    plugin: &str,
    policy: PasswordPolicy,
) {
    if pwd.is_empty() || plugin.eq_ignore_ascii_case("tidb_auth_token") {
        return;
    }
    if !history_enabled(policy) {
        return;
    }
    push_history(inner, key, pwd);
}

/// TiDB `checkPasswordReusePolicy` + `deleteHistoricalData` + `addHistoricalData`:
/// reject a password that contradicts the policy, prune rows that fell out of
/// the window, then record the new one.
fn verify_and_record_password(
    inner: &mut Store,
    key: &(String, String),
    pwd: &str,
    plugin: &str,
    policy: PasswordPolicy,
) -> Result<()> {
    if pwd.is_empty() || plugin.eq_ignore_ascii_case("tidb_auth_token") {
        return Ok(());
    }
    let rows: Vec<PasswordHistoryRow> = inner
        .password_history
        .iter()
        .filter(|row| row.user == key.0 && row.host == key.1)
        .cloned()
        .collect();
    // A disabled policy (explicit `PASSWORD HISTORY 0` / `PASSWORD REUSE
    // INTERVAL 0 DAY`, or globals of 0) both skips the reuse check and drops
    // the retained history: TiDB prunes every row it no longer has to keep.
    if !history_enabled(policy) {
        inner
            .password_history
            .retain(|row| !(row.user == key.0 && row.host == key.1));
        return Ok(());
    }
    let count = rows.len() as i64;
    // Maximum number of records the next insert may prune.
    let mut can_delete = count - policy.history + 1;
    if can_delete < 0 {
        can_delete = 0;
    }
    let mut allowed = true;
    if policy.history <= 0 && policy.reuse_days <= 0 {
        allowed = true;
    } else if count <= policy.history || policy.reuse_days > i32::MAX as i64 {
        allowed = !rows.iter().any(|row| row.password == pwd);
    } else {
        if policy.history > 0 {
            let mut newest = rows.clone();
            newest.sort_by_key(|row| std::cmp::Reverse(row.timestamp_micros));
            allowed = !newest
                .iter()
                .take(policy.history as usize)
                .any(|row| row.password == pwd);
        }
        if allowed && policy.reuse_days > 0 {
            let now = now_micros();
            let before = now.saturating_sub(policy.reuse_days.saturating_mul(86_400_000_000));
            allowed = !rows
                .iter()
                .any(|row| row.password == pwd && row.timestamp_micros >= before);
        }
    }
    if !allowed {
        return Err(Error::new(
            3638,
            format!(
                "Cannot use these credentials for '{}@{}' because they contradict the password history policy.",
                key.0, key.1
            ),
        ));
    }
    // Delete rows that both exceed the retention count and left the time window.
    if policy.reuse_days <= i32::MAX as i64 && can_delete > 0 {
        let now = now_micros();
        let before = if policy.reuse_days > 0 {
            now.saturating_sub(policy.reuse_days.saturating_mul(86_400_000_000))
        } else {
            i64::MAX
        };
        let mut candidates: Vec<(i64, usize)> = inner
            .password_history
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                row.user == key.0 && row.host == key.1 && row.timestamp_micros < before
            })
            .map(|(index, row)| (row.timestamp_micros, index))
            .collect();
        candidates.sort_unstable();
        let remove: Vec<usize> = candidates
            .into_iter()
            .take(can_delete as usize)
            .map(|(_, index)| index)
            .collect();
        for index in remove.into_iter().rev() {
            inner.password_history.remove(index);
        }
    }
    push_history(inner, key, pwd);
    Ok(())
}

fn encode_native_password(password: &str) -> String {
    if password.is_empty() {
        return String::new();
    }
    if password.starts_with('*') {
        return password.to_ascii_uppercase();
    }
    // mysql_native_password: `*` + uppercased hex(SHA1(SHA1(password))).
    let first = sha1_digest(password.as_bytes());
    let second = sha1_digest(&first);
    let mut out = String::with_capacity(41);
    out.push('*');
    for byte in second {
        out.push_str(&format!("{byte:02X}"));
    }
    out
}

/// Minimal SHA-1 (mysql_native_password storage hash).
fn sha1_digest(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// JSON string literal for `User_attributes` text.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
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

/// A `GRANT … ON … TO …` line for a raw static privilege bitmask (dynamic
/// privileges are emitted separately).
fn static_grant_line(
    bits: PrivilegeType,
    level: &str,
    user: &UserIdentity,
    grant_option: bool,
) -> String {
    let mut s = format!(
        "GRANT {} ON {} TO '{}'@'{}'",
        bits.names().join(","),
        level,
        user.username,
        user.hostname
    );
    if grant_option {
        s.push_str(" WITH GRANT OPTION");
    }
    s
}

thread_local! {
    /// Identity of the session executing on this thread. Sessions are pinned
    /// to a thread while a statement runs, so executors (e.g. `SHOW
    /// DATABASES`) can filter by the caller without threading the identity
    /// through every planner/executor signature (TiDB reads it from
    /// `sessionctx.Context`).
    static CURRENT_USER: std::cell::RefCell<Option<UserIdentity>> =
        const { std::cell::RefCell::new(None) };
    /// Roles activated by `SET ROLE` for the statement on this thread.
    /// `SHOW GRANTS` merges their privileges into the caller's output,
    /// mirroring TiDB's `ActiveRoles` (empty for `SHOW GRANTS FOR other`).
    static ACTIVE_ROLES: std::cell::RefCell<Vec<UserIdentity>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// Whether the statement runs under `sql_mode=NO_AUTO_CREATE_USER`:
    /// `GRANT` to a missing account then fails with 1410 instead of creating it.
    static NO_AUTO_CREATE_USER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub fn set_thread_user(user: Option<UserIdentity>) {
    CURRENT_USER.with(|cell| *cell.borrow_mut() = user);
}

pub fn set_thread_no_auto_create_user(value: bool) {
    NO_AUTO_CREATE_USER.with(|cell| cell.set(value));
}

pub fn thread_no_auto_create_user() -> bool {
    NO_AUTO_CREATE_USER.with(|cell| cell.get())
}

pub fn set_thread_active_roles(roles: Vec<UserIdentity>) {
    ACTIVE_ROLES.with(|cell| *cell.borrow_mut() = roles);
}

pub fn thread_user() -> Option<UserIdentity> {
    CURRENT_USER.with(|cell| cell.borrow().clone())
}

pub fn thread_active_roles() -> Vec<UserIdentity> {
    ACTIVE_ROLES.with(|cell| cell.borrow().clone())
}
