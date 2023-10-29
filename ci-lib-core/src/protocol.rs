use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "kind")]
#[serde(rename_all = "snake_case")]
pub enum ClientProto {
    Started,
    ArtifactCreate,
    NewTask(RequestedJob),
    NewTaskPlease { allowed_pushers: Option<Vec<String>>, host_info: HostInfo },
    Metric { name: String, value: String },
    Command(CommandInfo),
    TaskStatus(TaskInfo),
    Ping,
    Pong,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "command_info")]
#[serde(rename_all = "snake_case")]
pub enum CommandInfo {
    Started { command: Vec<String>, cwd: Option<String>, id: u32 },
    Finished { exit_code: Option<i32>, id: u32 },
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "task_info")]
#[serde(rename_all = "snake_case")]
pub enum TaskInfo {
    Finished { status: String },
    Interrupted { status: String, description: Option<String> },
}

impl ClientProto {
    pub fn metric(name: impl Into<String>, value: impl Into<String>) -> Self {
        ClientProto::Metric { name: name.into(), value: value.into() }
    }

    pub fn command(state: CommandInfo) -> Self {
        ClientProto::Command(state)
    }

    pub fn new_task_please(allowed_pushers: Option<Vec<String>>, host_info: HostInfo) -> Self {
        ClientProto::NewTaskPlease { allowed_pushers, host_info }
    }

    pub fn task_status(state: TaskInfo) -> Self {
        ClientProto::TaskStatus(state)
    }

    pub fn new_task(task: RequestedJob) -> Self {
        ClientProto::NewTask(task)
    }
}

impl CommandInfo {
    pub fn started(command: impl Into<Vec<String>>, cwd: Option<&str>, id: u32) -> Self {
        CommandInfo::Started { command: command.into(), cwd: cwd.map(ToOwned::to_owned), id }
    }

    pub fn finished(exit_code: Option<i32>, id: u32) -> Self {
        CommandInfo::Finished { exit_code, id }
    }
}

impl TaskInfo {
    pub fn finished(status: impl Into<String>) -> Self {
        TaskInfo::Finished { status: status.into() }
    }

    pub fn interrupted(status: impl Into<String>, description: impl Into<Option<String>>) -> Self {
        TaskInfo::Interrupted { status: status.into(), description: description.into() }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HostInfo {
    pub hostname: String,
    pub cpu_info: CpuInfo,
    pub memory_info: MemoryInfo,
    pub env_info: EnvInfo,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CpuInfo {
    pub model_name: String,
    pub microcode: String,
    pub cores: u32,
    pub vendor_id: String,
    pub family: String,
    pub model: String,
    // clock speed in khz
    pub max_freq: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MemoryInfo {
    pub total: String,
    pub available: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EnvInfo {
    pub arch: String,
    pub family: String,
    pub os: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestedJob {
    pub commit: String,
    pub remote_url: String,
    pub build_token: String,
}
