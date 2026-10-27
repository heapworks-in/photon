use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use db::sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection};

/// Ticks, drafts, dismissed findings and the verdict, per plan file (its absolute path).
pub struct PlanReviewDb(ThreadSafeConnection);

impl Domain for PlanReviewDb {
    const NAME: &str = "photon_plan_review";

    const MIGRATIONS: &[&str] = &["CREATE TABLE plan_ticks(
            plan_path TEXT NOT NULL,
            task_key TEXT NOT NULL,
            fingerprint TEXT NOT NULL,
            PRIMARY KEY(plan_path, task_key)
        ) STRICT;
        CREATE TABLE plan_drafts(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            plan_path TEXT NOT NULL,
            task_key TEXT NOT NULL,
            paragraph INTEGER NOT NULL,
            quote TEXT NOT NULL,
            body TEXT NOT NULL,
            finding TEXT
        ) STRICT;
        CREATE TABLE plan_dismissals(
            plan_path TEXT NOT NULL,
            finding_key TEXT NOT NULL,
            PRIMARY KEY(plan_path, finding_key)
        ) STRICT;
        CREATE TABLE plan_status(
            plan_path TEXT NOT NULL PRIMARY KEY,
            status TEXT NOT NULL
        ) STRICT;"];
}

db::static_connection!(PlanReviewDb, []);

/// A comment on one paragraph of a task, kept until the verdict is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanDraft {
    pub id: Option<i64>,
    pub task_key: String,
    pub paragraph: usize,
    /// The paragraph's text when the comment was written, to find it again after edits.
    pub quote: String,
    pub body: String,
    /// The finding this comment came from, when added from a finding card.
    pub finding: Option<String>,
}

impl PlanReviewDb {
    /// The fingerprint each ticked task had when it was ticked, by task key.
    pub fn ticks(&self, plan_path: &str) -> Result<HashMap<String, String>> {
        Ok(self.select_bound::<String, (String, String)>(
            "SELECT task_key, fingerprint FROM plan_ticks WHERE plan_path = ?1",
        )?(plan_path.to_string())?
        .into_iter()
        .collect())
    }

    /// `None` unticks the task.
    pub async fn set_tick(
        &self,
        plan_path: String,
        task_key: String,
        fingerprint: Option<String>,
    ) -> Result<()> {
        self.write(move |connection| match fingerprint {
            Some(fingerprint) => connection.exec_bound::<(String, String, String)>(
                "INSERT OR REPLACE INTO plan_ticks(plan_path, task_key, fingerprint)
                 VALUES (?1, ?2, ?3)",
            )?((plan_path, task_key, fingerprint)),
            None => connection.exec_bound::<(String, String)>(
                "DELETE FROM plan_ticks WHERE plan_path = ?1 AND task_key = ?2",
            )?((plan_path, task_key)),
        })
        .await
    }

    pub fn drafts(&self, plan_path: &str) -> Result<Vec<PlanDraft>> {
        self.select_bound::<String, (i64, String, i64, String, String, Option<String>)>(
            "SELECT id, task_key, paragraph, quote, body, finding FROM plan_drafts
             WHERE plan_path = ?1 ORDER BY id",
        )?(plan_path.to_string())?
        .into_iter()
        .map(|(id, task_key, paragraph, quote, body, finding)| {
            Ok(PlanDraft {
                id: Some(id),
                task_key,
                paragraph: usize::try_from(paragraph)?,
                quote,
                body,
                finding,
            })
        })
        .collect()
    }

    /// Inserts a draft without an id, otherwise updates it. Returns the id.
    pub async fn save_draft(&self, plan_path: String, draft: PlanDraft) -> Result<i64> {
        let paragraph = i64::try_from(draft.paragraph)?;
        let fields = (
            draft.task_key,
            paragraph,
            draft.quote,
            draft.body,
            draft.finding,
        );
        self.write(move |connection| match draft.id {
            Some(id) => {
                connection.exec_bound::<((String, i64, String, String, Option<String>), i64)>(
                    "UPDATE plan_drafts SET task_key = ?1, paragraph = ?2, quote = ?3, body = ?4,
                        finding = ?5
                     WHERE id = ?6",
                )?((fields, id))?;
                Ok(id)
            }
            None => connection
                .select_row_bound::<(String, (String, i64, String, String, Option<String>)), i64>(
                    "INSERT INTO plan_drafts(plan_path, task_key, paragraph, quote, body, finding)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     RETURNING id",
                )?((plan_path, fields))?
            .context("insert returned no id"),
        })
        .await
    }

    pub async fn delete_drafts(&self, ids: Vec<i64>) -> Result<()> {
        self.write(move |connection| {
            let mut delete =
                connection.exec_bound::<i64>("DELETE FROM plan_drafts WHERE id = ?1")?;
            for id in ids {
                delete(id)?;
            }
            Ok(())
        })
        .await
    }

    pub fn dismissed(&self, plan_path: &str) -> Result<HashSet<String>> {
        Ok(self.select_bound::<String, String>(
            "SELECT finding_key FROM plan_dismissals WHERE plan_path = ?1",
        )?(plan_path.to_string())?
        .into_iter()
        .collect())
    }

    pub async fn dismiss(&self, plan_path: String, finding_key: String) -> Result<()> {
        self.write(move |connection| {
            connection.exec_bound::<(String, String)>(
                "INSERT OR IGNORE INTO plan_dismissals(plan_path, finding_key) VALUES (?1, ?2)",
            )?((plan_path, finding_key))
        })
        .await
    }

    pub fn status(&self, plan_path: &str) -> Result<Option<String>> {
        self.select_row_bound::<String, String>(
            "SELECT status FROM plan_status WHERE plan_path = ?1",
        )?(plan_path.to_string())
    }

    pub async fn set_status(&self, plan_path: String, status: String) -> Result<()> {
        self.write(move |connection| {
            connection.exec_bound::<(String, String)>(
                "INSERT OR REPLACE INTO plan_status(plan_path, status) VALUES (?1, ?2)",
            )?((plan_path, status))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    async fn plan_state_round_trips(cx: &mut gpui::TestAppContext) {
        let db = cx
            .foreground_executor()
            .block_test(PlanReviewDb::open_test_db("plan_state_round_trips"));
        let plan = "/repo/docs/plan.md".to_string();
        db.set_tick(plan.clone(), "task-1".into(), Some("abc".into()))
            .await
            .unwrap();
        db.set_tick(plan.clone(), "task-2".into(), Some("def".into()))
            .await
            .unwrap();
        db.set_tick(plan.clone(), "task-2".into(), None)
            .await
            .unwrap();
        assert_eq!(
            db.ticks(&plan).unwrap(),
            HashMap::from_iter([("task-1".to_string(), "abc".to_string())])
        );
        assert!(db.ticks("/other.md").unwrap().is_empty());

        let mut draft = PlanDraft {
            id: None,
            task_key: "task-3".into(),
            paragraph: 4,
            quote: "Replace the call".into(),
            body: "Why?".into(),
            finding: None,
        };
        let id = db.save_draft(plan.clone(), draft.clone()).await.unwrap();
        draft.id = Some(id);
        draft.body = "Why not?".into();
        db.save_draft(plan.clone(), draft.clone()).await.unwrap();
        assert_eq!(db.drafts(&plan).unwrap(), [draft]);
        db.delete_drafts(vec![id]).await.unwrap();
        assert!(db.drafts(&plan).unwrap().is_empty());

        db.dismiss(plan.clone(), "no-test:task-6".into())
            .await
            .unwrap();
        db.dismiss(plan.clone(), "no-test:task-6".into())
            .await
            .unwrap();
        assert_eq!(db.dismissed(&plan).unwrap().len(), 1);

        assert_eq!(db.status(&plan).unwrap(), None);
        db.set_status(plan.clone(), "approved".into())
            .await
            .unwrap();
        assert_eq!(db.status(&plan).unwrap().as_deref(), Some("approved"));
    }
}
