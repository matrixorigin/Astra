use astra_core::SharedPool;

/// Delete one test owner's complete canonical Work aggregate in dependency order.
/// Call after owned execution writers have stopped. Slot-first locking matches
/// the background projector; one transaction prevents partially deleted Work
/// from becoming visible to that projector.
pub(crate) async fn cleanup_work_owner(pool: &SharedPool, owner_id: &str) {
    let mut transaction = pool
        .get()
        .begin()
        .await
        .expect("begin Work fixture cleanup");
    for (table, owner_column) in [
        ("work_runtime_event_outbox_slots", "owner_id"),
        ("work_runtime_event_outbox", "owner_id"),
        ("work_recovery_points", "owner_id"),
        ("work_patch_commit_operations", "owner_id"),
        ("work_patch_materialization_operations", "owner_id"),
        ("work_patch_artifacts", "owner_id"),
        ("session_artifact_references", "user_id"),
        ("session_artifacts", "user_id"),
        ("workspace_records", "owner_id"),
        ("work_item_attempts", "owner_id"),
        ("work_establishment_operations", "owner_id"),
        ("work_events", "owner_id"),
        ("work_attention_receipts", "owner_id"),
        ("work_event_sequences", "owner_id"),
        ("work_current_gap_acceptances", "owner_id"),
        ("work_acceptance_decisions", "owner_id"),
        ("work_check_runs", "owner_id"),
        ("work_proposals", "owner_id"),
        ("work_proposal_sequences", "owner_id"),
        ("work_branch_subjects", "owner_id"),
        ("work_branches", "owner_id"),
        ("work_item_edges", "owner_id"),
        ("work_item_revisions", "owner_id"),
        ("work_items", "owner_id"),
        ("work_graph_revisions", "owner_id"),
        ("work_graph_sequences", "owner_id"),
        ("work_criterion_sets", "owner_id"),
        ("work_criterion_revisions", "owner_id"),
        ("work_criteria", "owner_id"),
        ("work_goal_revisions", "owner_id"),
        ("works", "owner_id"),
        ("agent_sessions", "user_id"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {owner_column} = ?"
        )))
        .bind(owner_id)
        .execute(&mut *transaction)
        .await
        .unwrap_or_else(|error| panic!("clean {table}: {error}"));
    }
    transaction
        .commit()
        .await
        .expect("commit Work fixture cleanup");
}

/// A real managed Git workspace and exported patch for recovery-driver tests.
/// The fixture uses the canonical Work and provider paths, not injected effects.
pub(crate) struct PatchRuntimeFixture {
    pub pool: SharedPool,
    pub owner: astra_services::work::WorkOwnerId,
    pub work: astra_services::work::WorkId,
    pub branch: astra_services::work::WorkBranchId,
    pub lifecycle: crate::AgenticRunLifecycleService,
    pub foreign: crate::AgenticRunLifecycleService,
    pub workspace: std::path::PathBuf,
    pub patch: astra_services::work::WorkPatchArtifact,
}

impl PatchRuntimeFixture {
    pub(crate) async fn new() -> Self {
        use astra_services::work::*;
        use astra_services::{
            DatabaseWorkspaceRecordStore, WorkspaceRecordEntry, WorkspaceRecordStore,
        };
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "explicit DB lane required"
        );
        let settings = astra_core::MatrixOneSettings::from_env();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::ensure_core_schema(&settings, &catalog)
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
        let directory = std::sync::Arc::new(tempfile::tempdir().unwrap());
        let lifecycle = Self::lifecycle(directory.clone(), "patch-executor-a", settings.clone());
        let foreign = Self::lifecycle(directory, "patch-executor-b", settings);
        let id = uuid::Uuid::new_v4().to_string();
        let owner = WorkOwnerId::parse(format!("patch-owner-{id}")).unwrap();
        let work = WorkId::parse(format!("patch-work-{id}")).unwrap();
        let branch = WorkBranchId::parse(format!("patch-branch-{id}")).unwrap();
        let session = InternalSessionId::parse(format!("patch-session-{id}")).unwrap();
        let repository = DatabaseWorkRepository::new(pool.clone());
        repository
            .create_genesis(
                WorkGenesis::new(WorkGenesisParts {
                    owner_id: owner.clone(),
                    work_id: work.clone(),
                    branch_id: branch.clone(),
                    session_id: session.clone(),
                    project_id: None,
                    original_intent_ref: OriginalIntentRef::parse(format!("patch-intent-{id}"))
                        .unwrap(),
                    goal: WorkGoal::parse("Apply and commit an exact reviewed patch.").unwrap(),
                    criteria: vec![NewWorkCriterion {
                        criterion_id: CriterionId::parse("review").unwrap(),
                        definition: CriterionDefinition::HumanReview {
                            statement: CriterionStatement::parse("Review the applied file.")
                                .unwrap(),
                        },
                    }],
                })
                .unwrap(),
            )
            .await
            .unwrap();
        let record = lifecycle
            .fixture_server_workspace(session.as_str())
            .unwrap();
        let workspace = std::path::PathBuf::from(&record.root_or_volume_ref);
        DatabaseWorkspaceRecordStore::new(pool.clone())
            .upsert_workspace_record(WorkspaceRecordEntry::new(
                owner.as_str(),
                Some(session.as_str().into()),
                None,
                record,
            ))
            .await
            .unwrap();
        Self::git_at(&workspace, &["init", "--quiet"]);
        std::fs::write(workspace.join("file.txt"), "before\n").unwrap();
        Self::git_at(&workspace, &["add", "file.txt"]);
        Self::git_at(&workspace, &["commit", "--quiet", "-m", "initial"]);
        std::fs::write(workspace.join("file.txt"), "after\n").unwrap();
        let binding = repository
            .load_branch_runtime_binding(&owner, &work, &branch)
            .await
            .unwrap();
        let revision =
            astra_tools::patch_materialization::observe_git_worktree_revision(&workspace)
                .await
                .unwrap();
        let subject = repository
            .set_branch_subject(WorkBranchSubjectChange {
                owner_id: owner.clone(),
                work_id: work.clone(),
                branch_id: branch.clone(),
                expected_branch_revision: binding.branch_revision,
                graph_revision: binding.graph_revision,
                subject_ref: WorkSubjectRef::parse(format!("workspace/{}", session.as_str()))
                    .unwrap(),
                subject_revision: revision,
                source_ref: WorkChangeRef::parse("patch-export-subject").unwrap(),
            })
            .await
            .unwrap();
        let patch = super::work_patch_export_runtime::export_work_patch(
            pool.clone(),
            &lifecycle,
            super::work_patch_export_runtime::WorkPatchExportCommand {
                owner_id: owner.clone(),
                work_id: work.clone(),
                branch_id: branch.clone(),
                request_id: WorkChangeRef::parse("fixture-export").unwrap(),
                expected_branch_revision: subject.branch_revision,
                expected_graph_revision: subject.graph_revision,
            },
        )
        .await
        .unwrap();
        Self {
            pool,
            owner,
            work,
            branch,
            lifecycle,
            foreign,
            workspace,
            patch,
        }
    }

    fn lifecycle(
        directory: std::sync::Arc<tempfile::TempDir>,
        executor: &str,
        settings: astra_core::MatrixOneSettings,
    ) -> crate::AgenticRunLifecycleService {
        crate::AgenticRunLifecycleService::new(
            settings,
            std::sync::Arc::new(
                crate::FernetTokenEncryptor::new("cJ8pxr3t6iJmSYqe6wD7vu2rN_C3ovGUxkC5H3NXFNY=")
                    .unwrap(),
            ),
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            crate::RunEngine::new(std::sync::Arc::new(
                astra_services::InMemoryRunStateStore::new(),
            )),
        )
        .with_fixture_workspace_provider(directory, executor)
    }

    pub(crate) fn git(&self, args: &[&str]) -> String {
        Self::git_at(&self.workspace, args)
    }

    fn git_at(root: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Astra Test")
            .env("GIT_AUTHOR_EMAIL", "astra@example.invalid")
            .env("GIT_COMMITTER_NAME", "Astra Test")
            .env("GIT_COMMITTER_EMAIL", "astra@example.invalid")
            .output()
            .unwrap();
        assert!(output.status.success(), "Git fixture failed: {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    pub(crate) async fn reset_to_patch_base(&self) {
        use astra_services::work::*;
        self.git(&["checkout", "--", "file.txt"]);
        let repository = DatabaseWorkRepository::new(self.pool.clone());
        let binding = repository
            .load_branch_runtime_binding(&self.owner, &self.work, &self.branch)
            .await
            .unwrap();
        repository
            .set_branch_subject(WorkBranchSubjectChange {
                owner_id: self.owner.clone(),
                work_id: self.work.clone(),
                branch_id: self.branch.clone(),
                expected_branch_revision: binding.branch_revision,
                graph_revision: binding.graph_revision,
                subject_ref: self.patch.subject_ref.clone(),
                subject_revision: self.patch.base_subject_revision.clone(),
                source_ref: WorkChangeRef::parse("patch-base-subject").unwrap(),
            })
            .await
            .unwrap();
    }
}

pub(crate) async fn patch_operation_ownership(
    pool: &SharedPool,
    table: &str,
    operation: &str,
) -> (String, Option<String>, String) {
    assert!(matches!(
        table,
        "work_patch_commit_operations" | "work_patch_materialization_operations"
    ));
    sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT operation_phase, executor_token, DATE_FORMAT(recovery_after, '%Y-%m-%dT%H:%i:%s.%fZ') FROM {table} WHERE operation_id = ?")))
        .bind(operation).fetch_one(pool.get()).await.unwrap()
}
