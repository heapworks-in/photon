use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context as _, Result};
use db::sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection};

use crate::{Answer, LocalRun, PrKey, Side};

pub struct ReviewDb(ThreadSafeConnection);

pub const SUMMARY_SLOT: &str = "summary";

pub fn reply_slot(thread_id: &str) -> String {
    format!("reply:{thread_id}")
}

pub fn checklist_note_slot(item_key: &str) -> String {
    format!("check:{item_key}")
}

impl Domain for ReviewDb {
    const NAME: &str = "photon_review";

    const MIGRATIONS: &[&str] = &[
        "CREATE TABLE IF NOT EXISTS reviews(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            node_id TEXT NOT NULL,
            repo_dir TEXT NOT NULL,
            worktree TEXT NOT NULL,
            last_review_commit TEXT,
            PRIMARY KEY(host, owner, repo, number)
        ) STRICT;",
        "CREATE TABLE drafts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT NOT NULL,
            side TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            end_line INTEGER NOT NULL,
            commit_sha TEXT NOT NULL,
            anchor_text TEXT NOT NULL,
            body TEXT NOT NULL,
            outdated INTEGER NOT NULL,
            general INTEGER NOT NULL
        ) STRICT;
        CREATE TABLE viewed_steps(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            step_key TEXT NOT NULL,
            PRIMARY KEY(host, owner, repo, number, step_key)
        ) STRICT;",
        "ALTER TABLE drafts ADD COLUMN sending INTEGER NOT NULL DEFAULT 0;",
        "CREATE TABLE composer_texts(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            slot TEXT NOT NULL,
            body TEXT NOT NULL,
            PRIMARY KEY(host, owner, repo, number, slot)
        ) STRICT;",
        "CREATE TABLE checklist_answers(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            item_key TEXT NOT NULL,
            answer INTEGER NOT NULL,
            PRIMARY KEY(host, owner, repo, number, item_key)
        ) STRICT;
        CREATE TABLE local_runs(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            head_sha TEXT NOT NULL,
            command TEXT NOT NULL,
            exit_code INTEGER NOT NULL,
            PRIMARY KEY(host, owner, repo, number)
        ) STRICT;",
        "ALTER TABLE viewed_steps ADD COLUMN head_sha TEXT;
        ALTER TABLE viewed_steps ADD COLUMN fingerprint TEXT;
        CREATE TABLE viewed_files(
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            number INTEGER NOT NULL,
            path TEXT NOT NULL,
            head_sha TEXT NOT NULL,
            PRIMARY KEY(host, owner, repo, number, path)
        ) STRICT;",
        "ALTER TABLE drafts ADD COLUMN agent INTEGER NOT NULL DEFAULT 0;
        CREATE TABLE agent_dismissals(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            host TEXT NOT NULL,
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            path TEXT NOT NULL,
            body TEXT NOT NULL,
            reason TEXT NOT NULL
        ) STRICT;",
    ];
}

db::static_connection!(ReviewDb, []);

/// When a step was ticked. Both are `None` for marks from before Photon recorded them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ViewedMark {
    pub head: Option<String>,
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracked {
    pub key: PrKey,
    pub node_id: String,
    pub repo_dir: PathBuf,
    pub worktree: PathBuf,
}

/// A review comment the user is writing. Lines are 1-based; `Side::Left` lines are base lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    pub id: Option<i64>,
    pub key: PrKey,
    pub path: String,
    pub side: Side,
    pub start_line: u32,
    pub end_line: u32,
    pub commit: String,
    pub anchor_text: String,
    pub body: String,
    pub outdated: bool,
    pub general: bool,
    pub post_state: PostState,
    /// Written by the agent pre-review, so deleting it asks why.
    pub from_agent: bool,
}

/// Why an agent finding was dismissed, fed back to later pre-reviews of the repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DismissReason {
    Intentional,
    Wrong,
    NotWorthIt,
}

impl DismissReason {
    pub const ALL: [DismissReason; 3] = [
        DismissReason::Intentional,
        DismissReason::Wrong,
        DismissReason::NotWorthIt,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DismissReason::Intentional => "Intentional",
            DismissReason::Wrong => "Wrong",
            DismissReason::NotWorthIt => "Not worth it",
        }
    }

    fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|reason| reason.label() == label)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dismissal {
    pub path: String,
    pub body: String,
    pub reason: DismissReason,
}

/// Later prompts carry this many, so no more are kept.
pub const KEPT_DISMISSALS: usize = 20;

/// Stored in the `sending` column as 0, 1 or 2.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PostState {
    #[default]
    Unposted,
    /// Set while Submit posts the draft. Still set after a crash or a failure the review didn't
    /// see, so the draft may already be on GitHub.
    Sending,
    /// Edited while its review was posting: the earlier text is on GitHub, this one isn't.
    EarlierVersionPosted,
}

impl PostState {
    fn from_column(value: i64) -> Self {
        match value {
            1 => PostState::Sending,
            2 => PostState::EarlierVersionPosted,
            _ => PostState::Unposted,
        }
    }

    fn to_column(self) -> i64 {
        match self {
            PostState::Unposted => 0,
            PostState::Sending => 1,
            PostState::EarlierVersionPosted => 2,
        }
    }
}

// Per thread so a test's injected failures can't hit a test running in parallel.
#[cfg(test)]
thread_local! {
    /// Saves that fail before reaching the database, for testing the retry path.
    pub(crate) static FAILING_SAVES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

type DraftRow = (
    (i64, String, String, i64, i64),
    (String, String, String, bool, bool, i64, bool),
);

type TrackedRow = (String, String, String, i64, String, String, String);

impl ReviewDb {
    /// Keeps `last_review_commit` when the pull request is already tracked.
    pub async fn track(&self, tracked: Tracked) -> Result<()> {
        let number = i64::try_from(tracked.key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<TrackedRow>(
                "INSERT INTO reviews(host, owner, repo, number, node_id, repo_dir, worktree)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(host, owner, repo, number) DO UPDATE SET
                    node_id = excluded.node_id,
                    repo_dir = excluded.repo_dir,
                    worktree = excluded.worktree",
            )?((
                tracked.key.host,
                tracked.key.owner,
                tracked.key.repo,
                number,
                tracked.node_id,
                tracked.repo_dir.to_string_lossy().into_owned(),
                tracked.worktree.to_string_lossy().into_owned(),
            ))
        })
        .await
    }

    pub fn tracked(&self) -> Result<Vec<Tracked>> {
        self.select::<TrackedRow>(
            "SELECT host, owner, repo, number, node_id, repo_dir, worktree FROM reviews
             ORDER BY host, owner, repo, number",
        )?()?
        .into_iter()
        .map(|(host, owner, repo, number, node_id, repo_dir, worktree)| {
            Ok(Tracked {
                key: PrKey {
                    host,
                    owner,
                    repo,
                    number: u64::try_from(number)?,
                },
                node_id,
                repo_dir: PathBuf::from(repo_dir),
                worktree: PathBuf::from(worktree),
            })
        })
        .collect()
    }

    pub async fn untrack(&self, key: PrKey) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, i64)>(
                "DELETE FROM reviews WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((key.host, key.owner, key.repo, number))
        })
        .await
    }

    pub fn last_review(&self, key: &PrKey) -> Result<Option<String>> {
        let number = i64::try_from(key.number)?;
        let rows = self.select_bound::<(String, String, String, i64), Option<String>>(
            "SELECT last_review_commit FROM reviews
             WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
        )?((
            key.host.clone(),
            key.owner.clone(),
            key.repo.clone(),
            number,
        ))?;
        Ok(rows.into_iter().next().flatten())
    }

    pub async fn set_last_review(&self, key: PrKey, commit: String) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, i64, String)>(
                "UPDATE reviews SET last_review_commit = ?5
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((key.host, key.owner, key.repo, number, commit))
        })
        .await
    }

    pub fn drafts(&self, key: &PrKey) -> Result<Vec<Draft>> {
        let number = i64::try_from(key.number)?;
        self.select_bound::<(String, String, String, i64), DraftRow>(
            "SELECT id, path, side, start_line, end_line,
                    commit_sha, anchor_text, body, outdated, general, sending, agent
             FROM drafts WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4
             ORDER BY id",
        )?((
            key.host.clone(),
            key.owner.clone(),
            key.repo.clone(),
            number,
        ))?
        .into_iter()
        .map(
            |(
                (id, path, side, start_line, end_line),
                (commit, anchor_text, body, outdated, general, sending, from_agent),
            )| {
                Ok(Draft {
                    id: Some(id),
                    key: key.clone(),
                    path,
                    side: match side.as_str() {
                        "LEFT" => Side::Left,
                        _ => Side::Right,
                    },
                    start_line: u32::try_from(start_line)?,
                    end_line: u32::try_from(end_line)?,
                    commit,
                    anchor_text,
                    body,
                    outdated,
                    general,
                    post_state: PostState::from_column(sending),
                    from_agent,
                })
            },
        )
        .collect()
    }

    /// Inserts a draft without an id, otherwise updates the row with its id. Returns the id.
    pub async fn save_draft(&self, draft: Draft) -> Result<i64> {
        #[cfg(test)]
        if let Some(remaining) = FAILING_SAVES.get().checked_sub(1) {
            FAILING_SAVES.set(remaining);
            anyhow::bail!("injected save failure");
        }
        let number = i64::try_from(draft.key.number)?;
        let side = match draft.side {
            Side::Left => "LEFT",
            Side::Right => "RIGHT",
        };
        let fields = (
            (
                draft.path,
                side.to_string(),
                i64::from(draft.start_line),
                i64::from(draft.end_line),
            ),
            (
                draft.commit,
                draft.anchor_text,
                draft.body,
                draft.outdated,
                draft.general,
                draft.post_state.to_column(),
                draft.from_agent,
            ),
        );
        let key = draft.key;
        self.write(move |connection| match draft.id {
            Some(id) => {
                connection.exec_bound::<(_, i64)>(
                    "UPDATE drafts SET path = ?1, side = ?2, start_line = ?3, end_line = ?4,
                        commit_sha = ?5, anchor_text = ?6, body = ?7, outdated = ?8, general = ?9,
                        sending = ?10, agent = ?11
                     WHERE id = ?12",
                )?((fields, id))?;
                Ok(id)
            }
            None => connection.select_row_bound::<_, i64>(
                "INSERT INTO drafts(host, owner, repo, number, path, side, start_line,
                        end_line, commit_sha, anchor_text, body, outdated, general, sending, agent)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
                     RETURNING id",
            )?(((key.host, key.owner, key.repo, number), fields))?
            .context("insert returned no id"),
        })
        .await
    }

    pub async fn delete_draft(&self, id: i64) -> Result<()> {
        self.delete_drafts(vec![id]).await
    }

    pub async fn delete_drafts(&self, ids: Vec<i64>) -> Result<()> {
        self.write(move |connection| {
            let mut delete = connection.exec_bound::<i64>("DELETE FROM drafts WHERE id = ?1")?;
            for id in ids {
                delete(id)?;
            }
            Ok(())
        })
        .await
    }

    /// Unsent composer text by slot: `SUMMARY_SLOT`, or `reply_slot(thread_id)`.
    pub fn composer_texts(&self, key: &PrKey) -> Result<HashMap<String, String>> {
        let number = i64::try_from(key.number)?;
        Ok(
            self.select_bound::<(String, String, String, i64), (String, String)>(
                "SELECT slot, body FROM composer_texts
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((
                key.host.clone(),
                key.owner.clone(),
                key.repo.clone(),
                number,
            ))?
            .into_iter()
            .collect(),
        )
    }

    /// Blank text removes the slot's row.
    pub async fn save_composer_text(&self, key: PrKey, slot: String, body: String) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            if body.trim().is_empty() {
                connection.exec_bound::<(String, String, String, i64, String)>(
                    "DELETE FROM composer_texts
                     WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4 AND slot = ?5",
                )?((key.host, key.owner, key.repo, number, slot))
            } else {
                connection.exec_bound::<(String, String, String, i64, String, String)>(
                    "INSERT INTO composer_texts(host, owner, repo, number, slot, body)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(host, owner, repo, number, slot) DO UPDATE SET body = excluded.body",
                )?((key.host, key.owner, key.repo, number, slot, body))
            }
        })
        .await
    }

    pub fn viewed_steps(&self, key: &PrKey) -> Result<HashMap<String, ViewedMark>> {
        let number = i64::try_from(key.number)?;
        Ok(self
            .select_bound::<(String, String, String, i64), (String, Option<String>, Option<String>)>(
                "SELECT step_key, head_sha, fingerprint FROM viewed_steps
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((
                key.host.clone(),
                key.owner.clone(),
                key.repo.clone(),
                number,
            ))?
            .into_iter()
            .map(|(step_key, head, fingerprint)| (step_key, ViewedMark { head, fingerprint }))
            .collect())
    }

    /// `None` unticks the step.
    pub async fn set_step_viewed(
        &self,
        key: PrKey,
        step_key: String,
        mark: Option<ViewedMark>,
    ) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| match mark {
            Some(mark) => connection.exec_bound::<(
                (String, String, String, i64, String),
                (Option<String>, Option<String>),
            )>(
                "INSERT OR REPLACE INTO viewed_steps(host, owner, repo, number, step_key, head_sha, fingerprint)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?((
                (key.host, key.owner, key.repo, number, step_key),
                (mark.head, mark.fingerprint),
            )),
            None => connection.exec_bound::<(String, String, String, i64, String)>(
                "DELETE FROM viewed_steps
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4 AND step_key = ?5",
            )?((key.host, key.owner, key.repo, number, step_key)),
        })
        .await
    }

    /// The head each file was marked viewed at on GitHub from Photon, by path.
    pub fn viewed_files(&self, key: &PrKey) -> Result<HashMap<String, String>> {
        let number = i64::try_from(key.number)?;
        Ok(
            self.select_bound::<(String, String, String, i64), (String, String)>(
                "SELECT path, head_sha FROM viewed_files
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((
                key.host.clone(),
                key.owner.clone(),
                key.repo.clone(),
                number,
            ))?
            .into_iter()
            .collect(),
        )
    }

    /// `None` forgets the file's head, for a file unmarked as viewed.
    pub async fn set_file_viewed_head(
        &self,
        key: PrKey,
        path: String,
        head: Option<String>,
    ) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| match head {
            Some(head) => connection.exec_bound::<(String, String, String, i64, String, String)>(
                "INSERT OR REPLACE INTO viewed_files(host, owner, repo, number, path, head_sha)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?((key.host, key.owner, key.repo, number, path, head)),
            None => connection.exec_bound::<(String, String, String, i64, String)>(
                "DELETE FROM viewed_files
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4 AND path = ?5",
            )?((key.host, key.owner, key.repo, number, path)),
        })
        .await
    }
}

impl ReviewDb {
    /// The repository's dismissed agent findings, newest first.
    pub fn dismissals(&self, key: &PrKey) -> Result<Vec<Dismissal>> {
        Ok(
            self.select_bound::<(String, String, String), (String, String, String)>(
                "SELECT path, body, reason FROM agent_dismissals
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3
                 ORDER BY id DESC",
            )?((key.host.clone(), key.owner.clone(), key.repo.clone()))?
            .into_iter()
            .filter_map(|(path, body, reason)| {
                Some(Dismissal {
                    path,
                    body,
                    reason: DismissReason::from_label(&reason)?,
                })
            })
            .collect(),
        )
    }

    /// Keeps only the repository's newest `KEPT_DISMISSALS`.
    pub async fn add_dismissal(&self, key: PrKey, dismissal: Dismissal) -> Result<()> {
        let kept = i64::try_from(KEPT_DISMISSALS)?;
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, String, String, String)>(
                "INSERT INTO agent_dismissals(host, owner, repo, path, body, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?((
                key.host.clone(),
                key.owner.clone(),
                key.repo.clone(),
                dismissal.path,
                dismissal.body,
                dismissal.reason.label().to_string(),
            ))?;
            connection.exec_bound::<(String, String, String, i64)>(
                "DELETE FROM agent_dismissals
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND id NOT IN (
                    SELECT id FROM agent_dismissals
                    WHERE host = ?1 AND owner = ?2 AND repo = ?3
                    ORDER BY id DESC LIMIT ?4
                 )",
            )?((key.host, key.owner, key.repo, kept))
        })
        .await
    }

    pub fn checklist_answers(&self, key: &PrKey) -> Result<HashMap<String, Answer>> {
        let number = i64::try_from(key.number)?;
        Ok(
            self.select_bound::<(String, String, String, i64), (String, i64)>(
                "SELECT item_key, answer FROM checklist_answers
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
            )?((
                key.host.clone(),
                key.owner.clone(),
                key.repo.clone(),
                number,
            ))?
            .into_iter()
            .filter_map(|(item_key, answer)| Some((item_key, Answer::from_i64(answer)?)))
            .collect(),
        )
    }

    /// `None` clears the answer.
    pub async fn set_checklist_answer(
        &self,
        key: PrKey,
        item_key: String,
        answer: Option<Answer>,
    ) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| match answer {
            Some(answer) => connection.exec_bound::<(String, String, String, i64, String, i64)>(
                "INSERT OR REPLACE INTO checklist_answers(host, owner, repo, number, item_key, answer)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?((
                key.host,
                key.owner,
                key.repo,
                number,
                item_key,
                answer.to_i64(),
            )),
            None => connection.exec_bound::<(String, String, String, i64, String)>(
                "DELETE FROM checklist_answers
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4 AND item_key = ?5",
            )?((key.host, key.owner, key.repo, number, item_key)),
        })
        .await
    }

    pub fn local_run(&self, key: &PrKey) -> Result<Option<LocalRun>> {
        let number = i64::try_from(key.number)?;
        let rows = self.select_bound::<(String, String, String, i64), (String, String, i64)>(
            "SELECT head_sha, command, exit_code FROM local_runs
                 WHERE host = ?1 AND owner = ?2 AND repo = ?3 AND number = ?4",
        )?((
            key.host.clone(),
            key.owner.clone(),
            key.repo.clone(),
            number,
        ))?;
        Ok(rows
            .into_iter()
            .next()
            .map(|(head_sha, command, exit_code)| LocalRun {
                head_sha,
                command,
                exit_code: i32::try_from(exit_code).unwrap_or(i32::MAX),
            }))
    }

    pub async fn set_local_run(&self, key: PrKey, run: LocalRun) -> Result<()> {
        let number = i64::try_from(key.number)?;
        self.write(move |connection| {
            connection.exec_bound::<(String, String, String, i64, String, String, i64)>(
                "INSERT OR REPLACE INTO local_runs(host, owner, repo, number, head_sha, command, exit_code)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?((
                key.host,
                key.owner,
                key.repo,
                number,
                run.head_sha,
                run.command,
                i64::from(run.exit_code),
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(number: u64) -> PrKey {
        PrKey {
            host: "github.com".into(),
            owner: "acme".into(),
            repo: "app".into(),
            number,
        }
    }

    fn tracked(number: u64) -> Tracked {
        Tracked {
            key: key(number),
            node_id: format!("PR_{number}"),
            repo_dir: PathBuf::from("/repos/app"),
            worktree: PathBuf::from(format!("/wt/{number}")),
        }
    }

    #[gpui::test]
    async fn checklist_answers_and_local_runs_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("checklist_round_trip"));
        assert!(db.checklist_answers(&key(7)).unwrap().is_empty());
        db.set_checklist_answer(key(7), "a".into(), Some(Answer::No))
            .await
            .unwrap();
        db.set_checklist_answer(key(7), "b".into(), Some(Answer::Yes))
            .await
            .unwrap();
        db.set_checklist_answer(key(7), "b".into(), Some(Answer::NotApplicable))
            .await
            .unwrap();
        db.set_checklist_answer(key(8), "a".into(), Some(Answer::Yes))
            .await
            .unwrap();
        db.set_checklist_answer(key(7), "a".into(), None)
            .await
            .unwrap();
        assert_eq!(
            db.checklist_answers(&key(7)).unwrap(),
            HashMap::from_iter([("b".to_string(), Answer::NotApplicable)])
        );

        assert_eq!(db.local_run(&key(7)).unwrap(), None);
        let run = |command: &str, exit_code| LocalRun {
            head_sha: "h1".into(),
            command: command.into(),
            exit_code,
        };
        db.set_local_run(key(7), run("cargo test", 101))
            .await
            .unwrap();
        db.set_local_run(key(7), run("cargo test", 0))
            .await
            .unwrap();
        assert_eq!(db.local_run(&key(7)).unwrap(), Some(run("cargo test", 0)));
    }

    #[gpui::test]
    async fn tracked_and_last_review_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("tracked_round_trip"));
        assert_eq!(db.tracked().unwrap(), vec![]);
        assert_eq!(db.last_review(&key(7)).unwrap(), None);

        db.track(tracked(7)).await.unwrap();
        db.track(tracked(9)).await.unwrap();
        assert_eq!(db.tracked().unwrap(), vec![tracked(7), tracked(9)]);
        assert_eq!(db.last_review(&key(7)).unwrap(), None);

        db.set_last_review(key(7), "abc123".into()).await.unwrap();
        assert_eq!(db.last_review(&key(7)).unwrap().as_deref(), Some("abc123"));

        let mut moved = tracked(7);
        moved.worktree = PathBuf::from("/wt/moved");
        db.track(moved.clone()).await.unwrap();
        assert_eq!(db.tracked().unwrap()[0], moved);
        assert_eq!(db.last_review(&key(7)).unwrap().as_deref(), Some("abc123"));

        db.untrack(key(7)).await.unwrap();
        assert_eq!(db.tracked().unwrap(), vec![tracked(9)]);
        assert_eq!(db.last_review(&key(7)).unwrap(), None);
    }

    #[gpui::test]
    async fn composer_texts_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("composer_texts_round_trip"));
        db.save_composer_text(key(7), "summary".into(), "Looks good".into())
            .await
            .unwrap();
        db.save_composer_text(key(7), "reply:PRRT_1".into(), "Agreed".into())
            .await
            .unwrap();
        db.save_composer_text(key(9), "summary".into(), "Other".into())
            .await
            .unwrap();
        db.save_composer_text(key(7), "summary".into(), "Looks great".into())
            .await
            .unwrap();
        let texts = db.composer_texts(&key(7)).unwrap();
        assert_eq!(
            texts.get("summary").map(String::as_str),
            Some("Looks great")
        );
        assert_eq!(
            texts.get("reply:PRRT_1").map(String::as_str),
            Some("Agreed")
        );
        assert_eq!(texts.len(), 2);

        db.save_composer_text(key(7), "summary".into(), "  ".into())
            .await
            .unwrap();
        let texts = db.composer_texts(&key(7)).unwrap();
        assert_eq!(texts.get("summary"), None);
        assert_eq!(texts.len(), 1);
    }

    fn draft(number: u64, path: &str, start_line: u32) -> Draft {
        Draft {
            id: None,
            key: key(number),
            path: path.into(),
            side: Side::Right,
            start_line,
            end_line: start_line + 1,
            commit: "abc".into(),
            anchor_text: "a\nb".into(),
            body: "Why?".into(),
            outdated: false,
            general: false,
            post_state: crate::PostState::Unposted,
            from_agent: false,
        }
    }

    #[gpui::test]
    async fn dismissals_are_per_repository_and_capped(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("dismissals_round_trip"));
        let dismissal = |index: usize, reason| Dismissal {
            path: format!("src/{index}.rs"),
            body: format!("Finding {index}"),
            reason,
        };
        for index in 0..KEPT_DISMISSALS + 2 {
            db.add_dismissal(key(7), dismissal(index, DismissReason::Wrong))
                .await
                .unwrap();
        }
        let mut other_repo = key(1);
        other_repo.repo = "other".into();
        db.add_dismissal(
            other_repo.clone(),
            dismissal(99, DismissReason::Intentional),
        )
        .await
        .unwrap();
        db.add_dismissal(key(8), dismissal(100, DismissReason::NotWorthIt))
            .await
            .unwrap();

        let stored = db.dismissals(&key(7)).unwrap();
        assert_eq!(stored.len(), KEPT_DISMISSALS);
        assert_eq!(stored[0], dismissal(100, DismissReason::NotWorthIt));
        assert_eq!(
            stored[1],
            dismissal(KEPT_DISMISSALS + 1, DismissReason::Wrong)
        );
        assert_eq!(
            db.dismissals(&other_repo).unwrap(),
            [dismissal(99, DismissReason::Intentional)]
        );
    }

    #[gpui::test]
    async fn drafts_round_trip(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(ReviewDb::open_test_db("drafts_round_trip"));
        assert_eq!(db.drafts(&key(7)).unwrap(), vec![]);

        let mut first = draft(7, "src/b.rs", 3);
        first.id = Some(db.save_draft(first.clone()).await.unwrap());
        let mut second = draft(7, "src/a.rs", 9);
        second.side = Side::Left;
        second.outdated = true;
        second.general = true;
        second.post_state = PostState::EarlierVersionPosted;
        second.from_agent = true;
        second.id = Some(db.save_draft(second.clone()).await.unwrap());
        let mut other = draft(8, "src/a.rs", 1);
        other.id = Some(db.save_draft(other.clone()).await.unwrap());
        assert_ne!(first.id, second.id);
        assert_eq!(
            db.drafts(&key(7)).unwrap(),
            vec![first.clone(), second.clone()]
        );

        first.body = "Why not?".into();
        first.start_line = 4;
        first.commit = "def".into();
        assert_eq!(
            db.save_draft(first.clone()).await.unwrap(),
            first.id.unwrap()
        );
        assert_eq!(
            db.drafts(&key(7)).unwrap(),
            vec![first.clone(), second.clone()]
        );

        first.post_state = PostState::Sending;
        db.save_draft(first.clone()).await.unwrap();
        assert_eq!(
            db.drafts(&key(7)).unwrap()[0].post_state,
            PostState::Sending
        );
        first.post_state = PostState::Unposted;
        db.save_draft(first.clone()).await.unwrap();

        db.delete_draft(second.id.unwrap()).await.unwrap();
        assert_eq!(db.drafts(&key(7)).unwrap(), vec![first.clone()]);
        db.delete_drafts(vec![first.id.unwrap(), other.id.unwrap()])
            .await
            .unwrap();
        assert_eq!(db.drafts(&key(7)).unwrap(), vec![]);
        assert_eq!(db.drafts(&key(8)).unwrap(), vec![]);

        assert!(db.viewed_steps(&key(7)).unwrap().is_empty());
        let mark = |head: &str| ViewedMark {
            head: Some(head.into()),
            fingerprint: Some(format!("print-{head}")),
        };
        db.set_step_viewed(key(7), "src/a.rs::f".into(), Some(mark("h1")))
            .await
            .unwrap();
        db.set_step_viewed(key(7), "src/a.rs::f".into(), Some(mark("h2")))
            .await
            .unwrap();
        db.set_step_viewed(key(7), "src/b.rs".into(), Some(mark("h1")))
            .await
            .unwrap();
        db.set_step_viewed(key(7), "src/b.rs".into(), None)
            .await
            .unwrap();
        assert_eq!(
            db.viewed_steps(&key(7)).unwrap(),
            HashMap::from_iter([("src/a.rs::f".to_string(), mark("h2"))])
        );
        assert!(db.viewed_steps(&key(8)).unwrap().is_empty());

        db.set_file_viewed_head(key(7), "src/a.rs".into(), Some("h1".into()))
            .await
            .unwrap();
        db.set_file_viewed_head(key(7), "src/a.rs".into(), Some("h2".into()))
            .await
            .unwrap();
        db.set_file_viewed_head(key(7), "src/b.rs".into(), Some("h1".into()))
            .await
            .unwrap();
        db.set_file_viewed_head(key(7), "src/b.rs".into(), None)
            .await
            .unwrap();
        assert_eq!(
            db.viewed_files(&key(7)).unwrap(),
            HashMap::from_iter([("src/a.rs".to_string(), "h2".to_string())])
        );
        assert!(db.viewed_files(&key(8)).unwrap().is_empty());
    }
}
