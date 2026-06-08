use crossbeam_channel as channel;
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct BoxScheduler {
    sender: Option<channel::Sender<Job>>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl BoxScheduler {
    pub fn new(worker_count: usize) -> Self {
        let (sender, receiver) = channel::unbounded::<Job>();
        let mut workers = Vec::new();

        for _ in 0..worker_count.max(1) {
            let receiver = receiver.clone();
            workers.push(thread::spawn(move || loop {
                match receiver.recv() {
                    Ok(job) => job(),
                    Err(_) => break,
                }
            }));
        }

        Self {
            sender: Some(sender),
            workers,
        }
    }

    pub fn submit<F>(&self, job: F) -> Result<(), String>
    where
        F: FnOnce() + Send + 'static,
    {
        self.sender
            .as_ref()
            .ok_or_else(|| "scheduler is closed".to_string())?
            .send(Box::new(job))
            .map_err(|_| "scheduler is closed".to_string())
    }
}

impl Drop for BoxScheduler {
    fn drop(&mut self) {
        self.sender.take();

        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
