use std::path::Path;

use serde_json::json;

use super::{
    ChangeLimits, NotionClient, NotionError, NotionTarget, PreparedWrite, RecurringInputs,
    RecurringMode, RecurringPolicy, RecurringReport, RecurringTarget, StateStore, TargetLock,
    WriteAccess, notion_code, revoke_recurring, state_code,
};
use crate::{
    import::{ExternalRecordKey, OriginKind},
    notion::{
        CourseSetupVerification, SyncInput, SyncKey,
        state::BindingStatus,
        trial::MOODLE_VERIFICATION_ASSIGNMENTS,
        wire::{checked_page_id, text_item},
    },
};

pub(in crate::notion) struct RecurringVerification<'a> {
    target: &'a NotionTarget,
    directory: &'a Path,
    destination: RecurringTarget,
    policy: RecurringPolicy,
}

impl<'a> RecurringVerification<'a> {
    pub(in crate::notion) fn sample(id: &str) -> ExternalRecordKey {
        assert!(id.len() == 12 && id.bytes().all(|b| b.is_ascii_hexdigit()));
        ExternalRecordKey {
            origin: OriginKind::Moodle,
            scope: "assignment-manager.verification.recurring.synthetic".into(),
            id: id.into(),
        }
    }

    pub(in crate::notion) fn new(
        target: &'a NotionTarget,
        directory: &'a Path,
        id: &str,
        max_updates: usize,
    ) -> Result<Self, NotionError> {
        let verification = CourseSetupVerification::new(target)?;
        let key = SyncKey::derive(&Self::sample(id)).map_err(|_| NotionError::Configuration)?;
        let limits = ChangeLimits {
            max_creates: 1,
            max_updates,
        };
        let policy = RecurringPolicy::new(
            verification.writer_bot_id(),
            Some((
                limits,
                ["synthetic".into(), "synthetic".into(), "synthetic".into()],
            )),
            Some(ChangeLimits {
                max_creates: 0,
                max_updates: 0,
            }),
        )?;
        Ok(Self {
            target,
            directory,
            destination: RecurringTarget::Verification(key),
            policy,
        })
    }

    pub(in crate::notion) fn key(&self) -> &SyncKey {
        let RecurringTarget::Verification(key) = &self.destination else {
            unreachable!()
        };
        key
    }

    pub(in crate::notion) fn approve(
        &self,
        reader: &NotionClient,
        writer: &NotionClient,
    ) -> Result<(), &'static str> {
        reader.approve_recurring(
            writer,
            self.target,
            &self.policy,
            self.directory,
            &self.destination,
        )
    }

    pub(in crate::notion) fn disable(&self) -> Result<(), &'static str> {
        revoke_recurring(self.directory, MOODLE_VERIFICATION_ASSIGNMENTS)
    }

    pub(in crate::notion) fn run(
        &self,
        reader: &NotionClient,
        load_writer: impl FnOnce() -> Result<NotionClient, &'static str>,
        moodle: SyncInput,
        webclass: Result<SyncInput, &'static str>,
    ) -> RecurringReport {
        reader.recurring_report(
            self.target,
            &self.policy,
            self.directory,
            RecurringMode::Apply,
            &self.destination,
            (load_writer, |_| RecurringInputs {
                moodle: Ok(moodle),
                webclass,
            }),
        )
    }

    /// Seed manual values on the single synthetic page created by this run.
    pub(in crate::notion) fn seed_manual(
        &self,
        writer: &NotionClient,
        note: &str,
        url: &str,
    ) -> Result<(), &'static str> {
        let _lock = TargetLock::acquire(self.directory).map_err(state_code)?;
        let store = StateStore::for_apply(
            &self.directory.join("state.sqlite3"),
            MOODLE_VERIFICATION_ASSIGNMENTS,
        )
        .map_err(state_code)?;
        let approval = store.recurring_approval().map_err(state_code)?;
        let expected = self.destination.fingerprint(self.target, &self.policy)?;
        if approval
            .as_deref()
            .and_then(|value| value.split_once(':'))
            .map(|(policy, _)| policy)
            != Some(expected.as_str())
        {
            return Err("write_not_approved");
        }
        self.destination
            .check_writer(writer, self.target, &self.policy)?;
        let id = store
            .binding(self.key())
            .map_err(state_code)?
            .filter(|binding| binding.status == BindingStatus::Linked)
            .and_then(|binding| binding.page_id)
            .ok_or("synthetic_binding_missing")?;
        let schema = writer.retrieve_schema(self.target).map_err(notion_code)?;
        let page = writer.retrieve_page(&id, &[]).map_err(notion_code)?;
        if checked_page_id(&page, &schema.assignments_id, &schema.assignments_database)
            .map_err(notion_code)?
            != id
            || schema.page_key(&page).map_err(notion_code)?.as_ref() != Some(self.key())
        {
            return Err("synthetic_page_mismatch");
        }
        if page["properties"]["状態"]["status"]["name"] == "進行中"
            || page["properties"]["メモ"]["rich_text"]
                .as_array()
                .is_none_or(|notes| !notes.is_empty())
            || !crate::notion::wire::page_property(
                &page,
                &self.target.properties.submission_url,
                "url",
            )
            .map_err(notion_code)?
            .is_null()
        {
            return Err("synthetic_fields_already_set");
        }
        let fields = json!({
            "状態": {"status": {"name": "進行中"}},
            "メモ": {"rich_text": [text_item(note)]},
            (&self.target.properties.submission_url): {"url": url}
        });
        let request = PreparedWrite::verification_patch(self.key().clone(), &id, fields);
        let capability = WriteAccess::VerificationSeed(VerificationSeed {
            writer,
            request: &request,
        });
        writer
            .send_write(&request, &capability)
            .map_err(notion_code)?;
        Ok(())
    }
}

pub(in crate::notion) struct VerificationSeed<'a> {
    writer: &'a NotionClient,
    request: &'a PreparedWrite,
}

impl VerificationSeed<'_> {
    pub(in crate::notion) fn permits(
        &self,
        writer: &NotionClient,
        request: &PreparedWrite,
    ) -> bool {
        std::ptr::eq(self.writer, writer)
            && request.method() == reqwest::Method::PATCH
            && request.target_id == MOODLE_VERIFICATION_ASSIGNMENTS
            && request == self.request
    }
}
