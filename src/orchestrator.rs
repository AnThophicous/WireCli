use std::sync::{mpsc, Arc, Mutex};
use std::thread;

type Job = Box<dyn FnOnce() + Send + 'static>;

pub struct BoxScheduler {
    sender: mpsc::Sender<Job>,
    workers: Vec<thread::JoinHandle<()>>,
}

impl BoxScheduler {
    pub fn new(worker_count: usize) -> Self {
        let (sender, receiver) = mpsc::channel::<Job>();
        let shared_receiver = Arc::new(Mutex::new(receiver));
        let mut workers = Vec::new();

        for _ in 0..worker_count.max(1) {
            let receiver = Arc::clone(&shared_receiver);
            workers.push(thread::spawn(move || loop {
                let job = {
                    let locked = receiver.lock().expect("scheduler receiver poisoned");
                    locked.recv()
                };

                match job {
                    Ok(job) => job(),
                    Err(_) => break,
                }
            }));
        }

        Self { sender, workers }
    }

    pub fn submit<F>(&self, job: F) -> Result<(), String>
    where
        F: FnOnce() + Send + 'static,
    {
        self.sender
            .send(Box::new(job))
            .map_err(|_| "scheduler is closed".to_string())
    }
}

impl Drop for BoxScheduler {
    fn drop(&mut self) {
        let (replacement_sender, _receiver) = mpsc::channel::<Job>();
        let old_sender = std::mem::replace(&mut self.sender, replacement_sender);
        drop(old_sender);

        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
