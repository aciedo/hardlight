use std::collections::HashMap;

use hardlight::*;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let config = ServerConfig::new_self_signed("localhost:8080");
    let server = Server::new(config, factory!(Handler));
    tokio::spawn(async move { server.run().await.unwrap()});

    let mut client =
        TodoClient::new_self_signed("localhost:8080", Compression::default());
    client.connect().await.unwrap();

    let tasks_to_create = vec![
        Task::new("Buy milk"),
        Task::new("Buy eggs"),
        Task::new("Buy bread"),
        Task::new("use arch btw"),
    ];

    print_tasks(&client).await;

    let len = tasks_to_create.len();
    let start = std::time::Instant::now();
    for task in tasks_to_create {
        client.create(task).await.unwrap();
    }
    println!("Created {} tasks in {:?}", len, start.elapsed());

    let task_id = client.create(Task::new("Buy cheese")).await.unwrap();

    print_tasks(&client).await;

    client.delete(vec![task_id]).await.unwrap();

    print_tasks(&client).await;

    client.mark_as_done(vec![0, 1]).await.unwrap();

    client.delete_done_tasks().await.unwrap();

    print_tasks(&client).await;

    client.delete_all().await.unwrap();

    print_tasks(&client).await;

    client.disconnect();
}

async fn print_tasks(client: &TodoClient) {
    let tasks = client.get_all().await.unwrap();
    if tasks.is_empty() {
        println!("No tasks");
    } else {
        for task in tasks {
            println!("{:?}", task);
        }
    }

    println!();
}

/// These RPC methods are executed on the server and can be called by clients.
#[rpc]
trait Todo {
    /// Create a new task and return its ID.
    async fn create(&self, task: Task) -> HandlerResult<u32>;
    /// Mark the task with the given ID as done.
    async fn mark_as_done(&self, id: Vec<u32>) -> HandlerResult<()>;
    /// Get tasks with the given IDs.
    async fn get(&self, ids: Vec<u32>) -> HandlerResult<HashMap<u32, Task>>;
    /// Get all tasks.
    async fn get_all(&self) -> HandlerResult<HashMap<u32, Task>>;
    /// Delete tasks with the given IDs.
    async fn delete(&self, ids: Vec<u32>) -> HandlerResult<()>;
    /// Delete all tasks.
    async fn delete_all(&self) -> HandlerResult<()>;
    /// Delete all done tasks.
    async fn delete_done_tasks(&self) -> HandlerResult<()>;
}

#[codable]
#[derive(Debug, PartialEq, Eq, Clone)]
enum TaskStatus {
    Todo,
    Done,
}

#[codable]
#[derive(Debug, PartialEq, Eq, Clone)]
struct Task {
    text: String,
    status: TaskStatus,
}

impl Task {
    fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            status: TaskStatus::Todo,
        }
    }
}

#[connection_state]
struct State {
    tasks: HashMap<u32, Task>,
    next_id: u32,
}

#[rpc_handler]
impl Todo for Handler {
    async fn create(&self, task: Task) -> HandlerResult<u32> {
        let mut state = self.state.lock();
        let id = state.next_id;
        state.next_id += 1;
        state.tasks.insert(id, task);
        println!("State: {:?}", *state);
        Ok(id)
    }
    async fn mark_as_done(&self, ids: Vec<u32>) -> HandlerResult<()> {
        let mut state = self.state.lock();
        for id in ids {
            if let Some(task) = state.tasks.get_mut(&id) {
                task.status = TaskStatus::Done;
            }
        }
        Ok(())
    }
    async fn get(&self, ids: Vec<u32>) -> HandlerResult<HashMap<u32, Task>> {
        let state = self.state.lock();
        let mut tasks = HashMap::new();
        for id in ids {
            if let Some(task) = state.tasks.get(&id) {
                tasks.insert(id, task.clone());
            }
        }
        Ok(tasks)
    }
    async fn get_all(&self) -> HandlerResult<HashMap<u32, Task>> {
        let state = self.state.lock();
        Ok(state.tasks.clone())
    }
    async fn delete(&self, ids: Vec<u32>) -> HandlerResult<()> {
        let mut state = self.state.lock();
        for id in ids {
            state.tasks.remove(&id);
        }
        Ok(())
    }
    async fn delete_all(&self) -> HandlerResult<()> {
        let mut state = self.state.lock();
        state.tasks.clear();
        Ok(())
    }
    async fn delete_done_tasks(&self) -> HandlerResult<()> {
        let mut state = self.state.lock();
        let mut ids = Vec::new();
        for (id, task) in state.tasks.iter() {
            if let TaskStatus::Done = task.status {
                ids.push(*id);
            }
        }
        for id in ids {
            state.tasks.remove(&id);
        }
        Ok(())
    }
}
