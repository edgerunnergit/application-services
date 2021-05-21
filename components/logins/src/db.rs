/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::error::*;
use crate::login::{Login, SyncStatus};
use crate::schema;
use crate::util;
use lazy_static::lazy_static;
use rusqlite::{
    named_params,
    types::{FromSql, ToSql},
    Connection, NO_PARAMS,
};
use serde_derive::*;
use sql_support::{self, ConnExt};
use sql_support::{SqlInterruptHandle, SqlInterruptScope};
use std::ops::Deref;
use std::path::Path;
use std::sync::{atomic::AtomicUsize, Arc};
use std::time::{Duration, Instant, SystemTime};
use sync_guid::Guid;
use url::{Host, Url};

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Default)]
pub struct MigrationPhaseMetrics {
    num_processed: u64,
    num_succeeded: u64,
    num_failed: u64,
    total_duration: u128,
    errors: Vec<String>,
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Default)]
pub struct MigrationMetrics {
    fixup_phase: MigrationPhaseMetrics,
    insert_phase: MigrationPhaseMetrics,
    num_processed: u64,
    num_succeeded: u64,
    num_failed: u64,
    total_duration: u128,
    errors: Vec<String>,
}

pub struct LoginDb {
    pub db: Connection,
    interrupt_counter: Arc<AtomicUsize>,
}

impl LoginDb {
    pub fn with_connection(db: Connection) -> Result<Self> {
        #[cfg(test)]
        {
            util::init_test_logging();
        }

        // `temp_store = 2` is required on Android to force the DB to keep temp
        // files in memory, since on Android there's no tmp partition. See
        // https://github.com/mozilla/mentat/issues/505. Ideally we'd only
        // do this on Android, or allow caller to configure it.
        db.set_pragma("temp_store", 2)?;

        let mut logins = Self {
            db,
            interrupt_counter: Arc::new(AtomicUsize::new(0)),
        };
        let tx = logins.db.transaction()?;
        schema::init(&tx)?;
        tx.commit()?;
        Ok(logins)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_connection(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::with_connection(Connection::open_in_memory()?)
    }

    // Migrate from a sqlcipher encrypted database to a plaintext database with individual fields
    // encrypted
    //
    // The salt arg is for IOS and other systems where the salt is stored externally.
    pub fn migrate_sqlcipher_db_to_plaintext(
        old_db_path: impl AsRef<Path>,
        new_db_path: impl AsRef<Path>,
        old_encryption_key: &str,
        new_encryption_key: &str,
        salt: Option<&str>,
    ) -> Result<()> {
        crate::migrate_sqlcipher_db::migrate_sqlcipher_db_to_plaintext(
            old_db_path,
            new_db_path,
            old_encryption_key,
            new_encryption_key,
            salt,
        )
    }

    pub fn new_interrupt_handle(&self) -> SqlInterruptHandle {
        SqlInterruptHandle::new(
            self.db.get_interrupt_handle(),
            self.interrupt_counter.clone(),
        )
    }

    #[inline]
    pub fn begin_interrupt_scope(&self) -> SqlInterruptScope {
        SqlInterruptScope::new(self.interrupt_counter.clone())
    }
}

impl ConnExt for LoginDb {
    #[inline]
    fn conn(&self) -> &Connection {
        &self.db
    }
}

impl Deref for LoginDb {
    type Target = Connection;
    #[inline]
    fn deref(&self) -> &Connection {
        &self.db
    }
}

// login specific stuff.

impl LoginDb {
    pub(crate) fn put_meta(&self, key: &str, value: &dyn ToSql) -> Result<()> {
        self.execute_named_cached(
            "REPLACE INTO loginsSyncMeta (key, value) VALUES (:key, :value)",
            named_params! { ":key": key, ":value": value },
        )?;
        Ok(())
    }

    pub(crate) fn get_meta<T: FromSql>(&self, key: &str) -> Result<Option<T>> {
        self.try_query_row(
            "SELECT value FROM loginsSyncMeta WHERE key = :key",
            named_params! { ":key": key },
            |row| Ok::<_, Error>(row.get(0)?),
            true,
        )
    }

    pub(crate) fn delete_meta(&self, key: &str) -> Result<()> {
        self.execute_named_cached(
            "DELETE FROM loginsSyncMeta WHERE key = :key",
            named_params! { ":key": key },
        )?;
        Ok(())
    }

    // It would be nice if this were a batch-ish api (e.g. takes a slice of records and finds dupes
    // for each one if they exist)... I can't think of how to write that query, though.
    // NOTE: currently used only by sync - maybe it should move to the sync engine?
    // It doesn't *feel* sync specific though?
    pub(crate) fn find_dupe(&self, l: &Login) -> Result<Option<Login>> {
        let form_submit_host_port = l
            .form_submit_url
            .as_ref()
            .and_then(|s| util::url_host_port(&s));
        let args = named_params! {
            ":hostname": l.hostname,
            ":http_realm": l.http_realm,
            ":username": l.username,
            ":form_submit": form_submit_host_port,
        };
        let mut query = format!(
            "SELECT {common}
             FROM loginsL
             WHERE hostname IS :hostname
               AND httpRealm IS :http_realm
               AND username IS :username",
            common = schema::COMMON_COLS,
        );
        if form_submit_host_port.is_some() {
            // Stolen from iOS
            query += " AND (formSubmitURL = '' OR (instr(formSubmitURL, :form_submit) > 0))";
        } else {
            query += " AND formSubmitURL IS :form_submit"
        }
        self.try_query_row(&query, args, |row| Login::from_row(row), false)
    }

    pub fn get_all(&self) -> Result<Vec<Login>> {
        let mut stmt = self.db.prepare_cached(&GET_ALL_SQL)?;
        let rows = stmt.query_and_then(NO_PARAMS, Login::from_row)?;
        rows.collect::<Result<_>>()
    }

    pub fn get_by_base_domain(&self, base_domain: &str) -> Result<Vec<Login>> {
        // We first parse the input string as a host so it is normalized.
        let base_host = match Host::parse(base_domain) {
            Ok(d) => d,
            Err(e) => {
                // don't log the input string as it's PII.
                log::warn!("get_by_base_domain was passed an invalid domain: {}", e);
                return Ok(vec![]);
            }
        };
        // We just do a linear scan. Another option is to have an indexed
        // reverse-host column or similar, but current thinking is that it's
        // extra complexity for (probably) zero actual benefit given the record
        // counts are expected to be so low.
        // A regex would probably make this simpler, but we don't want to drag
        // in a regex lib just for this.
        let mut stmt = self.db.prepare_cached(&GET_ALL_SQL)?;
        let rows = stmt
            .query_and_then(NO_PARAMS, Login::from_row)?
            .filter(|r| {
                let login = r
                    .as_ref()
                    .ok()
                    .and_then(|login| Url::parse(&login.hostname).ok());
                let this_host = login.as_ref().and_then(|url| url.host());
                match (&base_host, this_host) {
                    (Host::Domain(base), Some(Host::Domain(look))) => {
                        // a fairly long-winded way of saying
                        // `login.hostname == base_domain ||
                        //  login.hostname.ends_with('.' + base_domain);`
                        let mut rev_input = base.chars().rev();
                        let mut rev_host = look.chars().rev();
                        loop {
                            match (rev_input.next(), rev_host.next()) {
                                (Some(ref a), Some(ref b)) if a == b => continue,
                                (None, None) => return true, // exactly equal
                                (None, Some(ref h)) => return *h == '.',
                                _ => return false,
                            }
                        }
                    }
                    // ip addresses must match exactly.
                    (Host::Ipv4(base), Some(Host::Ipv4(look))) => *base == look,
                    (Host::Ipv6(base), Some(Host::Ipv6(look))) => *base == look,
                    // all "mismatches" in domain types are false.
                    _ => false,
                }
            });
        rows.collect::<Result<_>>()
    }

    pub fn get_by_id(&self, id: &str) -> Result<Option<Login>> {
        self.try_query_row(
            &GET_BY_GUID_SQL,
            &[(":guid", &id as &dyn ToSql)],
            Login::from_row,
            true,
        )
    }

    pub fn touch(&self, id: &str) -> Result<()> {
        let tx = self.unchecked_transaction()?;
        self.ensure_local_overlay_exists(id)?;
        self.mark_mirror_overridden(id)?;
        let now_ms = util::system_time_ms_i64(SystemTime::now());
        // As on iOS, just using a record doesn't flip it's status to changed.
        // TODO: this might be wrong for lockbox!
        self.execute_named_cached(
            "UPDATE loginsL
             SET timeLastUsed = :now_millis,
                 timesUsed = timesUsed + 1,
                 local_modified = :now_millis
             WHERE guid = :guid
                 AND is_deleted = 0",
            named_params! {
                ":now_millis": now_ms,
                ":guid": id,
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn add(&self, login: Login) -> Result<Login> {
        let mut login = self.fixup_and_check_for_dupes(login)?;

        let tx = self.unchecked_transaction()?;
        let now_ms = util::system_time_ms_i64(SystemTime::now());

        // Allow an empty GUID to be passed to indicate that we should generate
        // one. (Note that the FFI, does not require that the `id` field be
        // present in the JSON, and replaces it with an empty string if missing).
        if login.guid.is_empty() {
            login.guid = Guid::random()
        }

        // Fill in default metadata.
        if login.time_created == 0 {
            login.time_created = now_ms;
        }
        if login.time_password_changed == 0 {
            login.time_password_changed = now_ms;
        }
        if login.time_last_used == 0 {
            login.time_last_used = now_ms;
        }
        if login.times_used == 0 {
            login.times_used = 1;
        }

        let sql = format!(
            "INSERT OR IGNORE INTO loginsL (
                hostname,
                httpRealm,
                formSubmitURL,
                usernameField,
                passwordField,
                timesUsed,
                username,
                password,
                guid,
                timeCreated,
                timeLastUsed,
                timePasswordChanged,
                local_modified,
                is_deleted,
                sync_status
            ) VALUES (
                :hostname,
                :http_realm,
                :form_submit_url,
                :username_field,
                :password_field,
                :times_used,
                :username,
                :password,
                :guid,
                :time_created,
                :time_last_used,
                :time_password_changed,
                :local_modified,
                0, -- is_deleted
                {new} -- sync_status
            )",
            new = SyncStatus::New as u8
        );

        let rows_changed = self.execute_named(
            &sql,
            named_params! {
                ":hostname": login.hostname,
                ":http_realm": login.http_realm,
                ":form_submit_url": login.form_submit_url,
                ":username_field": login.username_field,
                ":password_field": login.password_field,
                ":username": login.username,
                ":password": login.password,
                ":guid": login.guid,
                ":time_created": login.time_created,
                ":times_used": login.times_used,
                ":time_last_used": login.time_last_used,
                ":time_password_changed": login.time_password_changed,
                ":local_modified": now_ms,
            },
        )?;
        if rows_changed == 0 {
            log::error!(
                "Record {:?} already exists (use `update` to update records, not add)",
                login.guid
            );
            throw!(ErrorKind::DuplicateGuid(login.guid.into_string()));
        }
        tx.commit()?;
        Ok(login)
    }

    pub fn import_multiple(&self, logins: &[Login]) -> Result<MigrationMetrics> {
        // Check if the logins table is empty first.
        let mut num_existing_logins =
            self.query_row::<i64, _, _>("SELECT COUNT(*) FROM loginsL", NO_PARAMS, |r| r.get(0))?;
        num_existing_logins +=
            self.query_row::<i64, _, _>("SELECT COUNT(*) FROM loginsM", NO_PARAMS, |r| r.get(0))?;
        if num_existing_logins > 0 {
            return Err(ErrorKind::NonEmptyTable.into());
        }
        let tx = self.unchecked_transaction()?;
        let now_ms = util::system_time_ms_i64(SystemTime::now());
        let import_start = Instant::now();
        let sql = format!(
            "INSERT OR IGNORE INTO loginsL (
                hostname,
                httpRealm,
                formSubmitURL,
                usernameField,
                passwordField,
                timesUsed,
                username,
                password,
                guid,
                timeCreated,
                timeLastUsed,
                timePasswordChanged,
                local_modified,
                is_deleted,
                sync_status
            ) VALUES (
                :hostname,
                :http_realm,
                :form_submit_url,
                :username_field,
                :password_field,
                :times_used,
                :username,
                :password,
                :guid,
                :time_created,
                :time_last_used,
                :time_password_changed,
                :local_modified,
                0, -- is_deleted
                {new} -- sync_status
            )",
            new = SyncStatus::New as u8
        );
        let import_start_total_logins: u64 = logins.len() as u64;
        let mut num_failed_fixup: u64 = 0;
        let mut num_failed_insert: u64 = 0;
        let mut fixup_phase_duration = Duration::new(0, 0);
        let mut fixup_errors: Vec<String> = Vec::new();
        let mut insert_errors: Vec<String> = Vec::new();

        for login in logins {
            // This is a little bit of hoop-jumping to avoid cloning each borrowed item
            // in order to *possibly* created a fixed-up version.
            let mut login = login;
            let maybe_fixed_login = login.maybe_fixup().and_then(|fixed| {
                match &fixed {
                    None => self.check_for_dupes(login)?,
                    Some(l) => self.check_for_dupes(&l)?,
                };
                Ok(fixed)
            });
            match &maybe_fixed_login {
                Ok(None) => {} // The provided login was fine all along
                Ok(Some(l)) => {
                    // We made a new, fixed-up Login.
                    login = l;
                }
                Err(e) => {
                    log::warn!("Skipping login {} as it is invalid ({}).", login.guid, e);
                    fixup_errors.push(e.label().into());
                    num_failed_fixup += 1;
                    continue;
                }
            };
            // Now we can safely insert it, knowing that it's valid data.
            let old_guid = &login.guid; // Keep the old GUID around so we can debug errors easily.
            let guid = if old_guid.is_valid_for_sync_server() {
                old_guid.clone()
            } else {
                Guid::random()
            };
            fixup_phase_duration = import_start.elapsed();
            match self.execute_named_cached(
                &sql,
                named_params! {
                    ":hostname": login.hostname,
                    ":http_realm": login.http_realm,
                    ":form_submit_url": login.form_submit_url,
                    ":username_field": login.username_field,
                    ":password_field": login.password_field,
                    ":username": login.username,
                    ":password": login.password,
                    ":guid": guid,
                    ":time_created": login.time_created,
                    ":times_used": login.times_used,
                    ":time_last_used": login.time_last_used,
                    ":time_password_changed": login.time_password_changed,
                    ":local_modified": now_ms,
                },
            ) {
                Ok(_) => log::info!("Imported {} (new GUID {}) successfully.", old_guid, guid),
                Err(e) => {
                    log::warn!("Could not import {} ({}).", old_guid, e);
                    insert_errors.push(Error::from(e).label().into());
                    num_failed_insert += 1;
                }
            };
        }
        tx.commit()?;

        let num_post_fixup = import_start_total_logins - num_failed_fixup;
        let num_failed = num_failed_fixup + num_failed_insert;
        let insert_phase_duration = import_start
            .elapsed()
            .checked_sub(fixup_phase_duration)
            .unwrap_or_else(|| Duration::new(0, 0));
        let mut all_errors = Vec::new();
        all_errors.extend(fixup_errors.clone());
        all_errors.extend(insert_errors.clone());

        let metrics = MigrationMetrics {
            fixup_phase: MigrationPhaseMetrics {
                num_processed: import_start_total_logins,
                num_succeeded: num_post_fixup,
                num_failed: num_failed_fixup,
                total_duration: fixup_phase_duration.as_millis(),
                errors: fixup_errors,
            },
            insert_phase: MigrationPhaseMetrics {
                num_processed: num_post_fixup,
                num_succeeded: num_post_fixup - num_failed_insert,
                num_failed: num_failed_insert,
                total_duration: insert_phase_duration.as_millis(),
                errors: insert_errors,
            },
            num_processed: import_start_total_logins,
            num_succeeded: import_start_total_logins - num_failed,
            num_failed,
            total_duration: fixup_phase_duration
                .checked_add(insert_phase_duration)
                .unwrap_or_else(|| Duration::new(0, 0))
                .as_millis(),
            errors: all_errors,
        };
        log::info!(
            "Finished importing logins with the following metrics: {:#?}",
            metrics
        );
        Ok(metrics)
    }

    pub fn update(&self, login: Login) -> Result<()> {
        let login = self.fixup_and_check_for_dupes(login)?;

        let tx = self.unchecked_transaction()?;
        // Note: These fail with DuplicateGuid if the record doesn't exist.
        self.ensure_local_overlay_exists(login.guid_str())?;
        self.mark_mirror_overridden(login.guid_str())?;

        let now_ms = util::system_time_ms_i64(SystemTime::now());

        let sql = format!(
            "UPDATE loginsL
             SET local_modified      = :now_millis,
                 timeLastUsed        = :now_millis,
                 -- Only update timePasswordChanged if, well, the password changed.
                 timePasswordChanged = (CASE
                     WHEN password = :password
                     THEN timePasswordChanged
                     ELSE :now_millis
                 END),
                 httpRealm           = :http_realm,
                 formSubmitURL       = :form_submit_url,
                 usernameField       = :username_field,
                 passwordField       = :password_field,
                 timesUsed           = timesUsed + 1,
                 username            = :username,
                 password            = :password,
                 hostname            = :hostname,
                 -- leave New records as they are, otherwise update them to `changed`
                 sync_status         = max(sync_status, {changed})
             WHERE guid = :guid",
            changed = SyncStatus::Changed as u8
        );

        self.db.execute_named(
            &sql,
            named_params! {
                ":hostname": login.hostname,
                ":username": login.username,
                ":password": login.password,
                ":http_realm": login.http_realm,
                ":form_submit_url": login.form_submit_url,
                ":username_field": login.username_field,
                ":password_field": login.password_field,
                ":guid": login.guid,
                ":now_millis": now_ms,
            },
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn check_valid_with_no_dupes(&self, login: &Login) -> Result<()> {
        login.check_valid()?;
        self.check_for_dupes(login)
    }

    pub fn fixup_and_check_for_dupes(&self, login: Login) -> Result<Login> {
        let login = login.fixup()?;
        self.check_for_dupes(&login)?;
        Ok(login)
    }

    pub fn check_for_dupes(&self, login: &Login) -> Result<()> {
        if self.dupe_exists(&login)? {
            throw!(InvalidLogin::DuplicateLogin);
        }
        Ok(())
    }

    pub fn dupe_exists(&self, login: &Login) -> Result<bool> {
        // Note: the query below compares the guids of the given login with existing logins
        //  to prevent a login from being considered a duplicate of itself (e.g. during updates).
        Ok(self.db.query_row_named(
            "SELECT EXISTS(
                SELECT 1 FROM loginsL
                WHERE is_deleted = 0
                    AND guid <> :guid
                    AND hostname = :hostname
                    AND NULLIF(username, '') = :username
                    AND (
                        formSubmitURL = :form_submit
                        OR
                        httpRealm = :http_realm
                    )

                UNION ALL

                SELECT 1 FROM loginsM
                WHERE is_overridden = 0
                    AND guid <> :guid
                    AND hostname = :hostname
                    AND NULLIF(username, '') = :username
                    AND (
                        formSubmitURL = :form_submit
                        OR
                        httpRealm = :http_realm
                    )
             )",
            named_params! {
                ":guid": &login.guid,
                ":hostname": &login.hostname,
                ":username": &login.username,
                ":http_realm": login.http_realm.as_ref(),
                ":form_submit": login.form_submit_url.as_ref(),
            },
            |row| row.get(0),
        )?)
    }

    pub fn potential_dupes_ignoring_username(&self, login: &Login) -> Result<Vec<Login>> {
        // Could be lazy_static-ed...
        lazy_static::lazy_static! {
            static ref DUPES_IGNORING_USERNAME_SQL: String = format!(
                "SELECT {common_cols} FROM loginsL
                WHERE is_deleted = 0
                    AND hostname = :hostname
                    AND (
                        formSubmitURL = :form_submit
                        OR
                        httpRealm = :http_realm
                    )

                UNION ALL

                SELECT {common_cols} FROM loginsM
                WHERE is_overridden = 0
                    AND hostname = :hostname
                    AND (
                        formSubmitURL = :form_submit
                        OR
                        httpRealm = :http_realm
                    )
                ",
                common_cols = schema::COMMON_COLS
            );
        }
        let mut stmt = self.db.prepare_cached(&DUPES_IGNORING_USERNAME_SQL)?;
        let params = named_params! {
            ":hostname": &login.hostname,
            ":http_realm": login.http_realm.as_ref(),
            ":form_submit": login.form_submit_url.as_ref(),
        };
        // Needs to be two lines for borrow checker
        let rows = stmt.query_and_then_named(params, Login::from_row)?;
        rows.collect()
    }

    pub fn exists(&self, id: &str) -> Result<bool> {
        Ok(self.db.query_row_named(
            "SELECT EXISTS(
                 SELECT 1 FROM loginsL
                 WHERE guid = :guid AND is_deleted = 0
                 UNION ALL
                 SELECT 1 FROM loginsM
                 WHERE guid = :guid AND is_overridden IS NOT 1
             )",
            named_params! { ":guid": id },
            |row| row.get(0),
        )?)
    }

    /// Delete the record with the provided id. Returns true if the record
    /// existed already.
    pub fn delete(&self, id: &str) -> Result<bool> {
        let tx = self.unchecked_transaction_imm()?;
        let exists = self.exists(id)?;
        let now_ms = util::system_time_ms_i64(SystemTime::now());

        // For IDs that have, mark is_deleted and clear sensitive fields
        self.execute_named(
            &format!(
                "UPDATE loginsL
                 SET local_modified = :now_ms,
                     sync_status = {status_changed},
                     is_deleted = 1,
                     password = '',
                     hostname = '',
                     username = ''
                 WHERE guid = :guid",
                status_changed = SyncStatus::Changed as u8
            ),
            named_params! { ":now_ms": now_ms, ":guid": id },
        )?;

        // Mark the mirror as overridden
        self.execute_named(
            "UPDATE loginsM SET is_overridden = 1 WHERE guid = :guid",
            named_params! { ":guid": id },
        )?;

        // If we don't have a local record for this ID, but do have it in the mirror
        // insert a tombstone.
        self.execute_named(&format!("
            INSERT OR IGNORE INTO loginsL
                    (guid, local_modified, is_deleted, sync_status, hostname, timeCreated, timePasswordChanged, password, username)
            SELECT   guid, :now_ms,        1,          {changed},   '',       timeCreated, :now_ms,                   '',       ''
            FROM loginsM
            WHERE guid = :guid",
            changed = SyncStatus::Changed as u8),
            named_params! { ":now_ms": now_ms, ":guid": id })?;
        tx.commit()?;
        Ok(exists)
    }

    fn mark_mirror_overridden(&self, guid: &str) -> Result<()> {
        self.execute_named_cached(
            "UPDATE loginsM SET is_overridden = 1 WHERE guid = :guid",
            named_params! { ":guid": guid },
        )?;
        Ok(())
    }

    fn ensure_local_overlay_exists(&self, guid: &str) -> Result<()> {
        let already_have_local: bool = self.db.query_row_named(
            "SELECT EXISTS(SELECT 1 FROM loginsL WHERE guid = :guid)",
            named_params! { ":guid": guid },
            |row| row.get(0),
        )?;

        if already_have_local {
            return Ok(());
        }

        log::debug!("No overlay; cloning one for {:?}.", guid);
        let changed = self.clone_mirror_to_overlay(guid)?;
        if changed == 0 {
            log::error!("Failed to create local overlay for GUID {:?}.", guid);
            throw!(ErrorKind::NoSuchRecord(guid.to_owned()));
        }
        Ok(())
    }

    fn clone_mirror_to_overlay(&self, guid: &str) -> Result<usize> {
        Ok(self
            .execute_named_cached(&*CLONE_SINGLE_MIRROR_SQL, &[(":guid", &guid as &dyn ToSql)])?)
    }

    // Wipe is called both by Sync and also exposed publically, so it's
    // implemented here.
    pub(crate) fn wipe(&self, scope: &SqlInterruptScope) -> Result<()> {
        let tx = self.unchecked_transaction()?;
        log::info!("Executing wipe on password engine!");
        let now_ms = util::system_time_ms_i64(SystemTime::now());
        scope.err_if_interrupted()?;
        self.execute_named(
            &format!(
                "
                UPDATE loginsL
                SET local_modified = :now_ms,
                    sync_status = {changed},
                    is_deleted = 1,
                    password = '',
                    hostname = '',
                    username = ''
                WHERE is_deleted = 0",
                changed = SyncStatus::Changed as u8
            ),
            named_params! { ":now_ms": now_ms },
        )?;
        scope.err_if_interrupted()?;

        self.execute("UPDATE loginsM SET is_overridden = 1", NO_PARAMS)?;
        scope.err_if_interrupted()?;

        self.execute_named(
            &format!("
                INSERT OR IGNORE INTO loginsL
                      (guid, local_modified, is_deleted, sync_status, hostname, timeCreated, timePasswordChanged, password, username)
                SELECT guid, :now_ms,        1,          {changed},   '',       timeCreated, :now_ms,             '',       ''
                FROM loginsM",
                changed = SyncStatus::Changed as u8),
            named_params! { ":now_ms": now_ms })?;
        scope.err_if_interrupted()?;
        tx.commit()?;
        Ok(())
    }

    pub fn wipe_local(&self) -> Result<()> {
        log::info!("Executing wipe_local on password engine!");
        let tx = self.unchecked_transaction()?;
        self.execute_all(&[
            "DELETE FROM loginsL",
            "DELETE FROM loginsM",
            "DELETE FROM loginsSyncMeta",
        ])?;
        tx.commit()?;
        Ok(())
    }
}

lazy_static! {
    static ref GET_ALL_SQL: String = format!(
        "SELECT {common_cols} FROM loginsL WHERE is_deleted = 0
         UNION ALL
         SELECT {common_cols} FROM loginsM WHERE is_overridden = 0",
        common_cols = schema::COMMON_COLS,
    );
    static ref GET_BY_GUID_SQL: String = format!(
        "SELECT {common_cols}
         FROM loginsL
         WHERE is_deleted = 0
           AND guid = :guid

         UNION ALL

         SELECT {common_cols}
         FROM loginsM
         WHERE is_overridden IS NOT 1
           AND guid = :guid
         ORDER BY hostname ASC

         LIMIT 1",
        common_cols = schema::COMMON_COLS,
    );
    pub static ref CLONE_ENTIRE_MIRROR_SQL: String = format!(
        "INSERT OR IGNORE INTO loginsL ({common_cols}, local_modified, is_deleted, sync_status)
         SELECT {common_cols}, NULL AS local_modified, 0 AS is_deleted, 0 AS sync_status
         FROM loginsM",
        common_cols = schema::COMMON_COLS,
    );
    static ref CLONE_SINGLE_MIRROR_SQL: String =
        format!("{} WHERE guid = :guid", &*CLONE_ENTIRE_MIRROR_SQL,);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_valid_with_no_dupes() {
        let db = LoginDb::open_in_memory().unwrap();
        db.add(Login {
            guid: "dummy_000001".into(),
            form_submit_url: Some("https://www.example.com".into()),
            hostname: "https://www.example.com".into(),
            http_realm: None,
            username: "test".into(),
            password: "test".into(),
            ..Login::default()
        })
        .unwrap();

        let unique_login_guid = Guid::empty();
        let unique_login = Login {
            guid: unique_login_guid.clone(),
            form_submit_url: None,
            hostname: "https://www.example.com".into(),
            http_realm: Some("https://www.example.com".into()),
            username: "test".into(),
            password: "test".into(),
            ..Login::default()
        };

        let duplicate_login = Login {
            guid: Guid::empty(),
            form_submit_url: Some("https://www.example.com".into()),
            hostname: "https://www.example.com".into(),
            http_realm: None,
            username: "test".into(),
            password: "test2".into(),
            ..Login::default()
        };

        let updated_login = Login {
            guid: unique_login_guid,
            form_submit_url: None,
            hostname: "https://www.example.com".into(),
            http_realm: Some("https://www.example.com".into()),
            username: "test".into(),
            password: "test4".into(),
            ..Login::default()
        };

        struct TestCase {
            login: Login,
            should_err: bool,
            expected_err: &'static str,
        }

        let test_cases = [
            TestCase {
                // unique_login should not error because it does not share the same hostname,
                // username, and formSubmitURL or httpRealm with the pre-existing login
                // (login with guid "dummy_000001").
                login: unique_login,
                should_err: false,
                expected_err: "",
            },
            TestCase {
                // duplicate_login has the same hostname, username, and formSubmitURL as a pre-existing
                // login (guid "dummy_000001") and duplicate_login has no guid value, i.e. its guid
                // doesn't match with that of a pre-existing record so it can't be considered update,
                // so it should error.
                login: duplicate_login,
                should_err: true,
                expected_err: "Invalid login: Login already exists",
            },
            TestCase {
                // updated_login is an update to unique_login (has the same guid) so it is not a dupe
                // and should not error.
                login: updated_login,
                should_err: false,
                expected_err: "",
            },
        ];

        for tc in &test_cases {
            let login_check = db.check_valid_with_no_dupes(&tc.login);
            if tc.should_err {
                assert!(&login_check.is_err());
                assert_eq!(&login_check.unwrap_err().to_string(), tc.expected_err)
            } else {
                assert!(&login_check.is_ok())
            }
        }
    }

    #[test]
    fn test_unicode_submit() {
        let db = LoginDb::open_in_memory().unwrap();
        db.add(Login {
            guid: "dummy_000001".into(),
            form_submit_url: Some("http://😍.com".into()),
            hostname: "http://😍.com".into(),
            http_realm: None,
            username: "😍".into(),
            username_field: "😍".into(),
            password: "😍".into(),
            password_field: "😍".into(),
            ..Login::default()
        })
        .unwrap();
        let fetched = db
            .get_by_id("dummy_000001")
            .expect("should work")
            .expect("should get a record");
        assert_eq!(fetched.hostname, "http://xn--r28h.com");
        assert_eq!(fetched.form_submit_url.unwrap(), "http://xn--r28h.com");
        assert_eq!(fetched.username, "😍");
        assert_eq!(fetched.username_field, "😍");
        assert_eq!(fetched.password, "😍");
        assert_eq!(fetched.password_field, "😍");
    }

    #[test]
    fn test_unicode_realm() {
        let db = LoginDb::open_in_memory().unwrap();
        db.add(Login {
            guid: "dummy_000001".into(),
            form_submit_url: None,
            hostname: "http://😍.com".into(),
            http_realm: Some("😍😍".into()),
            username: "😍".into(),
            password: "😍".into(),
            ..Login::default()
        })
        .unwrap();
        let fetched = db
            .get_by_id("dummy_000001")
            .expect("should work")
            .expect("should get a record");
        assert_eq!(fetched.hostname, "http://xn--r28h.com");
        assert_eq!(fetched.http_realm.unwrap(), "😍😍");
    }

    fn check_matches(db: &LoginDb, query: &str, expected: &[&str]) {
        let mut results = db
            .get_by_base_domain(query)
            .unwrap()
            .into_iter()
            .map(|l| l.hostname)
            .collect::<Vec<String>>();
        results.sort_unstable();
        let mut sorted = expected.to_owned();
        sorted.sort_unstable();
        assert_eq!(sorted, results);
    }

    fn check_good_bad(
        good: Vec<&str>,
        bad: Vec<&str>,
        good_queries: Vec<&str>,
        zero_queries: Vec<&str>,
    ) {
        let db = LoginDb::open_in_memory().unwrap();
        for h in good.iter().chain(bad.iter()) {
            db.add(Login {
                hostname: (*h).into(),
                http_realm: Some((*h).into()),
                password: "test".into(),
                ..Login::default()
            })
            .unwrap();
        }
        for query in good_queries {
            check_matches(&db, query, &good);
        }
        for query in zero_queries {
            check_matches(&db, query, &[]);
        }
    }

    #[test]
    fn test_get_by_base_domain_invalid() {
        check_good_bad(
            vec!["https://example.com"],
            vec![],
            vec![],
            vec!["invalid query"],
        );
    }

    #[test]
    fn test_get_by_base_domain() {
        check_good_bad(
            vec![
                "https://example.com",
                "https://www.example.com",
                "http://www.example.com",
                "http://www.example.com:8080",
                "http://sub.example.com:8080",
                "https://sub.example.com:8080",
                "https://sub.sub.example.com",
                "ftp://sub.example.com",
            ],
            vec![
                "https://badexample.com",
                "https://example.co",
                "https://example.com.au",
            ],
            vec!["example.com"],
            vec!["foo.com"],
        );
        // punycode! This is likely to need adjusting once we normalize
        // on insert.
        check_good_bad(
            vec![
                "http://xn--r28h.com", // punycoded version of "http://😍.com"
            ],
            vec!["http://💖.com"],
            vec!["😍.com", "xn--r28h.com"],
            vec![],
        );
    }

    #[test]
    fn test_get_by_base_domain_ipv4() {
        check_good_bad(
            vec!["http://127.0.0.1", "https://127.0.0.1:8000"],
            vec!["https://127.0.0.0", "https://example.com"],
            vec!["127.0.0.1"],
            vec!["127.0.0.2"],
        );
    }

    #[test]
    fn test_get_by_base_domain_ipv6() {
        check_good_bad(
            vec!["http://[::1]", "https://[::1]:8000"],
            vec!["https://[0:0:0:0:0:0:1:1]", "https://example.com"],
            vec!["[::1]", "[0:0:0:0:0:0:0:1]"],
            vec!["[0:0:0:0:0:0:1:2]"],
        );
    }

    #[test]
    fn test_delete() {
        let db = LoginDb::open_in_memory().unwrap();
        let _login = db
            .add(Login {
                hostname: "https://www.example.com".into(),
                http_realm: Some("https://www.example.com".into()),
                username: "test_user".into(),
                password: "test_password".into(),
                ..Login::default()
            })
            .unwrap();

        assert!(db.delete(_login.guid_str()).unwrap());

        let tombstone_exists: bool = db
            .query_row_named(
                "SELECT EXISTS(
                    SELECT 1 FROM loginsL
                    WHERE guid = :guid AND is_deleted = 1
                )",
                named_params! { ":guid": _login.guid_str() },
                |row| row.get(0),
            )
            .unwrap();

        assert!(tombstone_exists);
        assert!(!db.exists(_login.guid_str()).unwrap());
    }

    #[test]
    fn test_wipe() {
        let db = LoginDb::open_in_memory().unwrap();
        let login1 = db
            .add(Login {
                hostname: "https://www.example.com".into(),
                http_realm: Some("https://www.example.com".into()),
                username: "test_user_1".into(),
                password: "test_password_1".into(),
                ..Login::default()
            })
            .unwrap();

        let login2 = db
            .add(Login {
                hostname: "https://www.example2.com".into(),
                http_realm: Some("https://www.example2.com".into()),
                username: "test_user_1".into(),
                password: "test_password_2".into(),
                ..Login::default()
            })
            .unwrap();

        assert!(db.wipe(&db.begin_interrupt_scope()).is_ok());

        let expected_tombstone_count = 2;
        let actual_tombstone_count: i32 = db
            .query_row_named(
                "SELECT COUNT(guid)
                    FROM loginsL
                    WHERE guid IN (:guid1,:guid2)
                        AND is_deleted = 1",
                named_params! {
                    ":guid1": login1.guid_str(),
                    ":guid2": login2.guid_str(),
                },
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(expected_tombstone_count, actual_tombstone_count);
        assert!(!db.exists(login1.guid_str()).unwrap());
        assert!(!db.exists(login2.guid_str()).unwrap());
    }

    fn delete_logins(db: &LoginDb, guids: &[String]) -> Result<()> {
        sql_support::each_chunk(guids, |chunk, _| -> Result<()> {
            db.execute(
                &format!(
                    "DELETE FROM loginsL WHERE guid IN ({vars})",
                    vars = sql_support::repeat_sql_vars(chunk.len())
                ),
                chunk,
            )?;
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn test_import_multiple() {
        struct TestCase {
            logins: Vec<Login>,
            has_populated_metrics: bool,
            expected_metrics: MigrationMetrics,
        }

        let db = LoginDb::open_in_memory().unwrap();

        // Adding login to trigger non-empty table error
        let login = db
            .add(Login {
                hostname: "https://www.example.com".into(),
                http_realm: Some("https://www.example.com".into()),
                username: "test_user_1".into(),
                password: "test_password_1".into(),
                ..Login::default()
            })
            .unwrap();

        let import_with_populated_table = db.import_multiple(Vec::new().as_slice());
        assert!(import_with_populated_table.is_err());
        assert_eq!(
            import_with_populated_table.unwrap_err().to_string(),
            "The logins tables are not empty"
        );

        // Removing added login so the test cases below don't fail
        delete_logins(&db, &[login.guid.into_string()]).unwrap();

        // Setting up test cases
        let valid_login_guid1: Guid = Guid::random();
        let valid_login1 = Login {
            guid: valid_login_guid1,
            form_submit_url: Some("https://www.example.com".into()),
            hostname: "https://www.example.com".into(),
            http_realm: None,
            username: "test".into(),
            password: "test".into(),
            ..Login::default()
        };
        let valid_login_guid2: Guid = Guid::random();
        let valid_login2 = Login {
            guid: valid_login_guid2,
            form_submit_url: Some("https://www.example2.com".into()),
            hostname: "https://www.example2.com".into(),
            http_realm: None,
            username: "test2".into(),
            password: "test2".into(),
            ..Login::default()
        };
        let valid_login_guid3: Guid = Guid::random();
        let valid_login3 = Login {
            guid: valid_login_guid3,
            form_submit_url: Some("https://www.example3.com".into()),
            hostname: "https://www.example3.com".into(),
            http_realm: None,
            username: "test3".into(),
            password: "test3".into(),
            ..Login::default()
        };
        let duplicate_login_guid: Guid = Guid::random();
        let duplicate_login = Login {
            guid: duplicate_login_guid,
            form_submit_url: Some("https://www.example.com".into()),
            hostname: "https://www.example.com".into(),
            http_realm: None,
            username: "test".into(),
            password: "test2".into(),
            ..Login::default()
        };

        let duplicate_logins = vec![valid_login1.clone(), duplicate_login, valid_login2.clone()];

        let duplicate_logins_metrics = MigrationMetrics {
            fixup_phase: MigrationPhaseMetrics {
                num_processed: 3,
                num_succeeded: 2,
                num_failed: 1,
                errors: vec!["InvalidLogin::DuplicateLogin".into()],
                ..MigrationPhaseMetrics::default()
            },
            insert_phase: MigrationPhaseMetrics {
                num_processed: 2,
                num_succeeded: 2,
                ..MigrationPhaseMetrics::default()
            },
            num_processed: 3,
            num_succeeded: 2,
            num_failed: 1,
            errors: vec!["InvalidLogin::DuplicateLogin".into()],
            ..MigrationMetrics::default()
        };

        let valid_logins = vec![valid_login1, valid_login2, valid_login3];

        let valid_logins_metrics = MigrationMetrics {
            fixup_phase: MigrationPhaseMetrics {
                num_processed: 3,
                num_succeeded: 3,
                ..MigrationPhaseMetrics::default()
            },
            insert_phase: MigrationPhaseMetrics {
                num_processed: 3,
                num_succeeded: 3,
                ..MigrationPhaseMetrics::default()
            },
            num_processed: 3,
            num_succeeded: 3,
            ..MigrationMetrics::default()
        };

        let test_cases = [
            TestCase {
                logins: Vec::new(),
                has_populated_metrics: false,
                expected_metrics: MigrationMetrics {
                    ..MigrationMetrics::default()
                },
            },
            TestCase {
                logins: duplicate_logins,
                has_populated_metrics: true,
                expected_metrics: duplicate_logins_metrics,
            },
            TestCase {
                logins: valid_logins,
                has_populated_metrics: true,
                expected_metrics: valid_logins_metrics,
            },
        ];

        for tc in &test_cases {
            let import_result = db.import_multiple(tc.logins.as_slice());
            assert!(import_result.is_ok());

            let mut actual_metrics = import_result.unwrap();

            if tc.has_populated_metrics {
                let mut guids = Vec::new();
                for login in &tc.logins {
                    guids.push(login.clone().guid.into_string());
                }

                assert_eq!(
                    actual_metrics.num_processed,
                    tc.expected_metrics.num_processed
                );
                assert_eq!(
                    actual_metrics.num_succeeded,
                    tc.expected_metrics.num_succeeded
                );
                assert_eq!(actual_metrics.num_failed, tc.expected_metrics.num_failed);
                assert_eq!(actual_metrics.errors, tc.expected_metrics.errors);

                let phases = [
                    (
                        actual_metrics.fixup_phase,
                        tc.expected_metrics.fixup_phase.clone(),
                    ),
                    (
                        actual_metrics.insert_phase,
                        tc.expected_metrics.insert_phase.clone(),
                    ),
                ];

                for (actual, expected) in &phases {
                    assert_eq!(actual.num_processed, expected.num_processed);
                    assert_eq!(actual.num_succeeded, expected.num_succeeded);
                    assert_eq!(actual.num_failed, expected.num_failed);
                    assert_eq!(actual.errors, expected.errors);
                }

                // clearing the database for next test case
                delete_logins(&db, guids.as_slice()).unwrap();
            } else {
                // We could elaborate mock out the clock for tests...
                // or we could just set the duration fields to the right values!
                actual_metrics.total_duration = tc.expected_metrics.total_duration;
                actual_metrics.fixup_phase.total_duration =
                    tc.expected_metrics.fixup_phase.total_duration;
                actual_metrics.insert_phase.total_duration =
                    tc.expected_metrics.insert_phase.total_duration;
                assert_eq!(actual_metrics, tc.expected_metrics);
            }
        }
    }
}
