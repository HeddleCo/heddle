//! Pending harness intents and decisions share the run's observed revision.
use api::heddle::api::v2alpha1::{DecideRunPermissionRequest, RunPermission};

use super::*;
impl RunStore {
    /// Internal digest-only callers receive a short-lived opaque local prompt.
    pub fn request_permission(&self, run: &str, id: &str, digest: &[u8]) -> Result<()> {
        let mut record = RunPermission {
            id: id.into(),
            request_digest: digest.to_vec(),
            harness: "local".into(),
            tool_name: "permission".into(),
            canonical_input_json: b"null".to_vec(),
            expires_at: Some(Default::default()),
        };
        record
            .expires_at
            .as_mut()
            .context("permission deadline missing")?
            .seconds = chrono::Utc::now().timestamp() + 60;
        self.request_permission_record(run, &record)
    }
    pub fn request_permission_record(&self, run_id: &str, record: &RunPermission) -> Result<()> {
        valid_id(&record.id)?;
        let now = chrono::Utc::now().timestamp();
        let deadline = record
            .expires_at
            .as_ref()
            .context("permission deadline required")?;
        if record.request_digest.len() != 32
            || record.harness.is_empty()
            || record.tool_name.is_empty()
            || record.canonical_input_json.len() > 64 * 1024
            || deadline.nanos != 0
            || deadline.seconds <= now
            || deadline.seconds > now + 300
        {
            bail!("invalid bounded permission intent");
        }
        let _: serde_json::Value = serde_json::from_slice(&record.canonical_input_json)
            .context("invalid permission intent JSON")?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut run = load_run(&tx, run_id)?.context("permission run unavailable")?;
        tx.execute("INSERT INTO run_permissions(run,id,digest,record,expires) VALUES (?1,?2,?3,?4,?5) ON CONFLICT(run,id) DO NOTHING",params![run_id,record.id,record.request_digest,record.encode_to_vec(),deadline.seconds])?;
        let (stored, closed, decision): (Vec<u8>, bool, Option<bool>) = tx.query_row(
            "SELECT record,closed,decision FROM run_permissions WHERE run=?1 AND id=?2",
            params![run_id, record.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if stored != record.encode_to_vec() || closed || decision.is_some() {
            bail!("permission id reused or already retired");
        }
        populate(&tx, &mut run)?;
        persist(&tx, &run)?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
    pub fn permission_decision(&self, run: &str, id: &str, digest: &[u8]) -> Result<Option<bool>> {
        let row: Option<(Vec<u8>, Option<bool>, bool, i64)> = self
            .connection()?
            .query_row(
                "SELECT digest,decision,closed,expires FROM run_permissions WHERE run=?1 AND id=?2",
                params![run, id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (actual, decision, closed, expires) = row.context("permission unavailable")?;
        if actual != digest {
            bail!("permission request changed");
        }
        if closed || expires <= chrono::Utc::now().timestamp() {
            return Ok(None);
        }
        Ok(decision)
    }
    pub fn close_permission(&self, run_id: &str, id: &str, digest: &[u8]) -> Result<()> {
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if tx.execute(
            "UPDATE run_permissions SET closed=1 WHERE run=?1 AND id=?2 AND digest=?3",
            params![run_id, id, digest],
        )? != 1
        {
            bail!("permission absent or digest changed");
        }
        let mut run = load_run(&tx, run_id)?.context("run unavailable")?;
        populate(&tx, &mut run)?;
        persist(&tx, &run)?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
    pub fn decide_permission(
        &self,
        request: &DecideRunPermissionRequest,
        principal: &str,
    ) -> Result<()> {
        valid_id(&request.client_operation_id)?;
        let reference = request.run.as_ref().context("run required")?;
        let mut connection = self.connection()?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let body = request.encode_to_vec();
        if command_replay(
            &tx,
            &request.client_operation_id,
            "permission",
            &body,
            principal,
        )? {
            return self.committed();
        }
        let mut run = load_run(&tx, &reference.id)?.context("run unavailable")?;
        if run.r#ref.as_ref() != Some(reference) {
            bail!("run unavailable");
        }
        if tx.execute("UPDATE run_permissions SET decision=?4 WHERE run=?1 AND id=?2 AND digest=?3 AND decision IS NULL AND closed=0 AND expires>?5",params![reference.id,request.permission_id,request.request_digest,request.allow,chrono::Utc::now().timestamp()])?!=1 {bail!("permission absent, expired, closed, already decided, or digest changed");}
        populate(&tx, &mut run)?;
        persist(&tx, &run)?;
        record_command(
            &tx,
            &request.client_operation_id,
            "permission",
            &body,
            principal,
        )?;
        tx.commit()?;
        self.committed()?;
        Ok(())
    }
}
pub(super) fn populate(connection: &Connection, run: &mut RunRecord) -> Result<()> {
    let id = &run.r#ref.as_ref().context("run reference missing")?.id;
    let mut query=connection.prepare("SELECT record FROM run_permissions WHERE run=?1 AND closed=0 AND decision IS NULL AND expires>?2 ORDER BY id LIMIT 65")?;
    run.pending_permissions = query
        .query_map(params![id, chrono::Utc::now().timestamp()], |row| {
            row.get::<_, Vec<u8>>(0)
        })?
        .map(|row| Ok(RunPermission::decode(row?.as_slice())?))
        .collect::<Result<Vec<_>>>()?;
    if run.pending_permissions.len() > 64 || run.encode_to_vec().len() > 240 * 1024 {
        bail!("run pending permission budget exceeded");
    }
    Ok(())
}
fn persist(connection: &Connection, run: &RunRecord) -> Result<()> {
    let mut run = run.clone();
    run.version.clear();
    run.version = blake3::hash(&run.encode_to_vec()).as_bytes().to_vec();
    connection.execute(
        "UPDATE runs SET record=?2 WHERE id=?1",
        params![
            run.r#ref.as_ref().context("run reference missing")?.id,
            run.encode_to_vec()
        ],
    )?;
    Ok(())
}
pub(super) fn project(mut run: RunRecord) -> RunRecord {
    let before = run.pending_permissions.len();
    run.pending_permissions.retain(|permission| {
        permission
            .expires_at
            .as_ref()
            .is_some_and(|deadline| deadline.seconds > chrono::Utc::now().timestamp())
    });
    if run.pending_permissions.len() != before {
        run.version.clear();
        run.version = blake3::hash(&run.encode_to_vec()).as_bytes().to_vec();
    }
    run
}
