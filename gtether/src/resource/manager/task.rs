use async_task::Task;
use std::cmp::Ordering;
use std::fmt::{Display, Formatter};
use crate::util::executor::{Executor, StaticMetadata, StaticPriority};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskPriority {
    Immediate(i64),
    Delayed(i64),
    Update,
}

impl PartialOrd for TaskPriority {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TaskPriority {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Immediate(a), Self::Immediate(b)) |
            (Self::Delayed(a), Self::Delayed(b)) => {
                a.cmp(b)
            },
            (Self::Update, Self::Update) => {
                Ordering::Equal
            },
            (Self::Immediate(_), Self::Delayed(_) | Self::Update) |
            (Self::Delayed(_), Self::Update) => {
                Ordering::Greater
            },
            (Self::Delayed(_), Self::Immediate(_)) |
            (Self::Update, Self::Immediate(_) | Self::Delayed(_)) => {
                Ordering::Less
            },
        }
    }
}

impl Display for TaskPriority {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Immediate(val) => write!(f, "Immediate({val})"),
            Self::Delayed(val) => write!(f, "Delayed({val})"),
            Self::Update => write!(f, "Update"),
        }
    }
}

pub type ManagerTask<T> = Task<T, StaticMetadata<TaskPriority>>;
pub type ManagerExecutor = Executor<'static, StaticPriority<TaskPriority>>;