//! 本地前台任务日志，不派发任务、不自动重放，也不访问设备。
//! 介质副作用及提交结果由执行方记录；日志终态不代表副作用被回滚。
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use crate::error::{Result, TapeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    Running,
    Success,
    Failed,
    Interrupted,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Running
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRecord {
    pub id: i64,
    pub operation: String,
    pub target: String,
    pub state: TaskState,
    /// 完成比例，0–100；不作为提交已确认的凭据。
    pub progress: u8,
    pub message: String,
    /// 调用方提供的副作用/提交事实，未知时保持 None，不推断成功。
    pub outcome: Option<Value>,
    pub cancel_requested: bool,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

impl TaskRecord {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id, "operation": self.operation, "target": self.target,
            "state": self.state.as_str(), "progress": self.progress,
            "message": self.message, "outcome": self.outcome,
            "cancel_requested": self.cancel_requested, "created_at": self.created_at,
            "updated_at": self.updated_at, "finished_at": self.finished_at,
        })
    }
}

/// 与 Library 共用数据库的独立连接；不获取控制器锁。
pub struct TaskStore {
    db: Connection,
    lock_path: PathBuf,
}

const SELECT: &str = "SELECT id,operation,target,state,progress,message,outcome,
    cancel_requested,created_at,updated_at,finished_at FROM library_tasks";

fn refuse(reason: &str) -> TapeError {
    TapeError::Refused {
        reason: reason.into(),
    }
}

fn decode_error(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}

fn record(row: &Row<'_>) -> rusqlite::Result<TaskRecord> {
    let state: String = row.get(3)?;
    let state = match state.as_str() {
        "running" => TaskState::Running,
        "success" => TaskState::Success,
        "failed" => TaskState::Failed,
        "interrupted" => TaskState::Interrupted,
        "cancelled" => TaskState::Cancelled,
        _ => {
            return Err(decode_error(
                3,
                std::io::Error::new(std::io::ErrorKind::InvalidData, "未知任务状态"),
            ));
        }
    };
    let outcome: Option<String> = row.get(6)?;
    Ok(TaskRecord {
        id: row.get(0)?,
        operation: row.get(1)?,
        target: row.get(2)?,
        state,
        progress: row.get(4)?,
        message: row.get(5)?,
        outcome: outcome
            .map(|value| serde_json::from_str(&value).map_err(|e| decode_error(6, e)))
            .transpose()?,
        cancel_requested: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

impl TaskStore {
    /// 仅创建独立任务表，不更改已有库表，不推断旧 running 任务已经中断。
    pub fn open(state: &Path) -> Result<Self> {
        let db = Connection::open(state)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS library_tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                operation TEXT NOT NULL, target TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('running','success','failed','interrupted','cancelled')),
                progress INTEGER NOT NULL DEFAULT 0 CHECK(progress BETWEEN 0 AND 100),
                message TEXT NOT NULL DEFAULT '', outcome TEXT,
                cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0,1)),
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, finished_at TEXT
            );")?;
        let lock_path = std::env::current_dir()?
            .join(state)
            .with_extension("library-lock");
        Ok(Self { db, lock_path })
    }

    /// 按稳定整数 ID 升序返回当前任务和历史任务。
    pub fn list(&self) -> Result<Vec<TaskRecord>> {
        Ok(self
            .db
            .prepare(&format!("{SELECT} ORDER BY id"))?
            .query_map([], record)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn show(&self, id: i64) -> Result<TaskRecord> {
        self.db
            .query_row(&format!("{SELECT} WHERE id=?1"), [id], record)
            .optional()?
            .ok_or_else(|| refuse("任务不存在"))
    }

    /// 登记前台执行意图，不获取设备资格，不排队派发或恢复执行。
    pub fn start(&self, operation: &str, target: &str) -> Result<TaskRecord> {
        if operation.trim().is_empty() {
            return Err(refuse("任务操作名称不能为空"));
        }
        self.db.execute("INSERT INTO library_tasks(operation,target,state,created_at,updated_at)
            VALUES(?1,?2,'running',strftime('%Y-%m-%dT%H:%M:%fZ','now'),strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            params![operation, target])?;
        self.show(self.db.last_insert_rowid())
    }

    /// 安全检查点；返回 true 才能开始下一个阶段。None 保留已有 outcome。
    /// false 仅表示已有取消请求，仍保持 Running；调用方收尾后显式 finish。
    /// 执行方不得在 append 开始后、commit 完成前响应取消。
    pub fn checkpoint(
        &self,
        id: i64,
        progress: u8,
        message: &str,
        outcome: Option<&Value>,
    ) -> Result<bool> {
        if progress > 100 {
            return Err(refuse("任务进度必须在 0–100 之间"));
        }
        let keep_running = self
            .db
            .query_row(
                "UPDATE library_tasks SET progress=?2,message=?3,
            outcome=COALESCE(?4,outcome),updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now')
            WHERE id=?1 AND state='running' RETURNING cancel_requested=0",
                params![id, progress, message, outcome.map(Value::to_string)],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| refuse("任务不存在或已结束"))?;
        Ok(keep_running)
    }

    /// 仅登记协作取消请求；终态返回 false，未知 ID 返回错误。
    /// 请求不打断正在进行的设备命令，也不承诺回滚已有副作用。
    pub fn cancel(&self, id: i64) -> Result<bool> {
        let tx = self.db.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE library_tasks SET cancel_requested=1,
            updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND state='running'",
            [id],
        )?;
        if changed == 0 {
            self.show(id)?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    /// 查询请求，不改变状态；仅在调用方认可的安全点用于协作取消。
    pub fn cancel_requested(&self, id: i64) -> Result<bool> {
        Ok(self.show(id)?.cancel_requested)
    }

    /// 显式确认本地前实例已退出；排他锁未获得时不修改任何任务。
    /// 不读取磁带，不推断提交结果，不重放任何操作。
    pub fn recover_interrupted(&self, message: &str) -> Result<usize> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.lock_path)?;
        let _lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock)
            .map_err(|(_, error)| TapeError::Nix(error))?;
        self.interrupt_with_message(message)
    }

    /// 仅供 Library 在取得同一 state 的 controller lock 后调用。
    /// 不能用于普通任务查询、打开数据库或活动控制器之外的恢复。
    pub(crate) fn interrupt_running(&self) -> Result<usize> {
        self.interrupt_with_message("前台控制器已退出；介质副作用和提交结果须另行核验")
    }

    fn interrupt_with_message(&self, message: &str) -> Result<usize> {
        Ok(self.db.execute(
            "UPDATE library_tasks SET state='interrupted',message=?1,
            updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),
            finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE state='running'",
            [message],
        )?)
    }

    /// 仅清除终态历史；取消请求中的 running 任务仍由执行方负责结束。
    pub fn clear_history(&self) -> Result<usize> {
        Ok(self.db.execute(
            "DELETE FROM library_tasks WHERE state IN
            ('success','failed','interrupted','cancelled')",
            [],
        )?)
    }

    /// 记录执行方观察到的终态；取消请求不能抹去已完成的提交。
    pub fn finish(
        &self,
        id: i64,
        state: TaskState,
        message: &str,
        outcome: Option<&Value>,
    ) -> Result<TaskRecord> {
        if !state.is_terminal() {
            return Err(refuse("任务结束状态必须是终态"));
        }
        let tx = self.db.unchecked_transaction()?;
        let changed = tx.execute(
            "UPDATE library_tasks SET state=?2,message=?3,
            progress=CASE WHEN ?2='success' THEN 100 ELSE progress END,
            outcome=COALESCE(?4,outcome),updated_at=strftime('%Y-%m-%dT%H:%M:%fZ','now'),
            finished_at=strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id=?1 AND state='running'",
            params![id, state.as_str(), message, outcome.map(Value::to_string)],
        )?;
        if changed != 1 {
            return Err(refuse("任务不存在或已结束"));
        }
        let task = self.show(id)?;
        tx.commit()?;
        Ok(task)
    }
}
