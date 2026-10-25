//! Blob uploads: a small pool of threads, each with its own [`Remote`] handle.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use budwood_client::Remote;
use budwood_core::ObjectId;

type Job = (ObjectId, Vec<u8>);

pub struct Uploader {
    jobs: Option<Sender<Job>>,
    results: Receiver<Result<()>>,
    workers: Vec<JoinHandle<()>>,
}

impl Uploader {
    pub fn new(remote: &Remote, threads: usize) -> Self {
        let (jobs, job_rx) = channel::<Job>();
        let (result_tx, results) = channel();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let workers = (0..threads.max(1))
            .map(|_| {
                let (remote, job_rx, result_tx) = (remote.clone(), job_rx.clone(), result_tx.clone());
                std::thread::spawn(move || loop {
                    // The guard is dropped at the end of this statement, so
                    // only one idle worker waits on the channel at a time.
                    let job = job_rx.lock().unwrap().recv();
                    let Ok((id, data)) = job else { return };
                    let result = remote.put_blob(&id, data).with_context(|| format!("uploading blob {id}"));
                    if result_tx.send(result).is_err() {
                        return;
                    }
                })
            })
            .collect();
        Self { jobs: Some(jobs), results, workers }
    }

    /// Uploads every blob and waits for all of them; returns the first error.
    pub fn upload_all(&self, blobs: Vec<Job>) -> Result<()> {
        let n = blobs.len();
        let jobs = self.jobs.as_ref().unwrap();
        for blob in blobs {
            jobs.send(blob).context("upload workers exited")?;
        }
        let mut first_error = None;
        for _ in 0..n {
            if let Err(e) = self.results.recv().context("upload workers exited")? {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for Uploader {
    fn drop(&mut self) {
        self.jobs.take(); // Closing the channel stops the workers.
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}
