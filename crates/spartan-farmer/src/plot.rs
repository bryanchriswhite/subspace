mod commitments;
#[cfg(test)]
mod tests;

use crate::plot::commitments::Commitments;
use crate::{crypto, Piece, Salt, Tag, BATCH_SIZE, PIECE_SIZE};
use async_std::fs::OpenOptions;
use async_std::path::PathBuf;
use futures::channel::mpsc as async_mpsc;
use futures::channel::oneshot;
use futures::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SinkExt, StreamExt};
use log::{error, trace};
use rayon::prelude::*;
use rocksdb::DB;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io;
use std::io::SeekFrom;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use subspace_core_primitives::RootBlock;
use thiserror::Error;
use tokio::task::JoinHandle;

const LAST_ROOT_BLOCK_KEY: &[u8] = b"last_root_block";

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum CommitmentStatus {
    /// In-progress commitment to the part of the plot
    InProgress,
    /// Commitment to the whole plot and not some in-progress partial commitment
    Created,
    /// Commitment creation was aborted, waiting for cleanup
    Aborted,
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error)]
pub(crate) enum PlotError {
    #[error("Plot open error: {0}")]
    PlotOpen(io::Error),
    #[error("Plot DB open error: {0}")]
    PlotDbOpen(rocksdb::Error),
    #[error("Commitments open error: {0}")]
    CommitmentsOpen(io::Error),
}

#[derive(Debug)]
enum ReadRequests {
    ReadEncoding {
        index: u64,
        result_sender: oneshot::Sender<io::Result<Piece>>,
    },
    ReadEncodings {
        first_index: u64,
        count: u64,
        /// Vector containing all of the pieces as contiguous block of memory
        result_sender: oneshot::Sender<io::Result<Vec<u8>>>,
    },
    FindByRange {
        target: Tag,
        range: u64,
        salt: Salt,
        result_sender: oneshot::Sender<io::Result<Option<(Tag, u64)>>>,
    },
}

#[derive(Debug)]
enum WriteRequests {
    WriteEncodings {
        encodings: Vec<Piece>,
        first_index: u64,
        result_sender: oneshot::Sender<io::Result<()>>,
    },
    WriteTags {
        first_index: u64,
        tags: Vec<Tag>,
        salt: Salt,
        result_sender: oneshot::Sender<io::Result<()>>,
    },
    FinishCommitmentCreation {
        salt: Salt,
        result_sender: oneshot::Sender<()>,
    },
    RemoveCommitment {
        salt: Salt,
        result_sender: oneshot::Sender<()>,
    },
}

struct Inner {
    background_handle: Option<JoinHandle<Commitments>>,
    any_requests_sender: Option<async_mpsc::Sender<()>>,
    read_requests_sender: Option<async_mpsc::Sender<ReadRequests>>,
    write_requests_sender: Option<async_mpsc::Sender<WriteRequests>>,
    plot_db: Option<Arc<DB>>,
    piece_count: Arc<AtomicU64>,
    commitment_statuses: Mutex<HashMap<Salt, CommitmentStatus>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Close sending channels so that background future can actually exit
        self.any_requests_sender.take();
        self.read_requests_sender.take();
        self.write_requests_sender.take();
        let plot_db = self.plot_db.take();

        let background_handle = self.background_handle.take().unwrap();
        tokio::task::block_in_place(move || {
            tokio::runtime::Handle::current()
                .block_on(async move { background_handle.await })
                .unwrap();

            drop(plot_db);
        });
    }
}

/// `Plot` struct is an abstraction on top of both plot and tags database.
///
/// It converts async requests to internal reads/writes to the plot and tags database. It
/// prioritizes reads over writes by having separate queues for reads and writes requests, read
/// requests are executed until exhausted after which at most 1 write request is handled and the
/// cycle repeats. This allows finding solution with as little delay as possible while introducing
/// changes to the plot at the same time (re-plotting on salt changes or extending plot size).
#[derive(Clone)]
pub(crate) struct Plot {
    inner: Arc<Inner>,
}

impl Plot {
    /// Creates a new plot for persisting encoded pieces to disk
    pub(crate) async fn open_or_create(base_directory: &PathBuf) -> Result<Plot, PlotError> {
        let mut plot_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(base_directory.join("plot.bin"))
            .await
            .map_err(PlotError::PlotOpen)?;

        let plot_size = plot_file
            .metadata()
            .await
            .map_err(PlotError::PlotOpen)?
            .len();

        let piece_count = Arc::new(AtomicU64::new(plot_size / PIECE_SIZE as u64));

        let plot_db = tokio::task::spawn_blocking({
            let path = base_directory.join("plot-metadata");

            move || DB::open_default(path)
        })
        .await
        .unwrap()
        .map_err(PlotError::PlotDbOpen)?;

        // Channel with at most single element to throttle loop below if there are no updates
        let (any_requests_sender, mut any_requests_receiver) = async_mpsc::channel::<()>(1);
        let (read_requests_sender, mut read_requests_receiver) =
            async_mpsc::channel::<ReadRequests>(100);
        let (write_requests_sender, mut write_requests_receiver) =
            async_mpsc::channel::<WriteRequests>(100);

        let commitments_fut = Commitments::new(base_directory.join("commitments"));
        let mut commitments = commitments_fut.await.map_err(PlotError::CommitmentsOpen)?;
        let commitment_statuses: HashMap<Salt, CommitmentStatus> = commitments
            .get_existing_commitments()
            .map(|&salt| (salt, CommitmentStatus::Created))
            .collect();

        let background_handle = tokio::spawn({
            let piece_count = Arc::clone(&piece_count);

            async move {
                let mut did_nothing = true;
                'outer: loop {
                    if did_nothing {
                        // Wait for stuff to come in
                        if any_requests_receiver.next().await.is_none() {
                            break;
                        }
                    }

                    did_nothing = true;

                    // Process as many read requests as there is
                    while let Ok(read_request) = read_requests_receiver.try_next() {
                        did_nothing = false;

                        match read_request {
                            Some(ReadRequests::ReadEncoding {
                                index,
                                result_sender,
                            }) => {
                                let _ = result_sender.send(
                                    try {
                                        plot_file
                                            .seek(SeekFrom::Start(index * PIECE_SIZE as u64))
                                            .await?;
                                        let mut buffer = [0u8; PIECE_SIZE];
                                        plot_file.read_exact(&mut buffer).await?;
                                        buffer
                                    },
                                );
                            }
                            Some(ReadRequests::ReadEncodings {
                                first_index,
                                count,
                                result_sender,
                            }) => {
                                let _ = result_sender.send(
                                    try {
                                        plot_file
                                            .seek(SeekFrom::Start(first_index * PIECE_SIZE as u64))
                                            .await?;
                                        let mut buffer =
                                            Vec::with_capacity(count as usize * PIECE_SIZE);
                                        buffer.resize(buffer.capacity(), 0);
                                        plot_file.read_exact(&mut buffer).await?;
                                        buffer
                                    },
                                );
                            }
                            None => {
                                break 'outer;
                            }
                            Some(ReadRequests::FindByRange {
                                target,
                                range,
                                salt,
                                result_sender,
                            }) => {
                                let tags_db = match commitments.get_or_create_db(salt).await {
                                    Ok(tags_db) => tags_db,
                                    Err(error) => {
                                        error!("Failed to open tags database: {}", error);
                                        continue;
                                    }
                                };
                                // TODO: Remove unwrap
                                let solutions_fut = tokio::task::spawn_blocking(move || {
                                    let mut iter = tags_db.raw_iterator();

                                    let mut solutions: Vec<(Tag, u64)> = Vec::new();

                                    let (lower, is_lower_overflowed) =
                                        u64::from_be_bytes(target).overflowing_sub(range / 2);
                                    let (upper, is_upper_overflowed) =
                                        u64::from_be_bytes(target).overflowing_add(range / 2);

                                    trace!(
                                        "{} Lower overflow: {} -- Upper overflow: {}",
                                        u64::from_be_bytes(target),
                                        is_lower_overflowed,
                                        is_upper_overflowed
                                    );

                                    if is_lower_overflowed || is_upper_overflowed {
                                        iter.seek_to_first();
                                        while let Some(tag) = iter.key() {
                                            let tag = tag.try_into().unwrap();
                                            let index = iter.value().unwrap();
                                            if u64::from_be_bytes(tag) <= upper {
                                                solutions.push((
                                                    tag,
                                                    u64::from_le_bytes(index.try_into().unwrap()),
                                                ));
                                                iter.next();
                                            } else {
                                                break;
                                            }
                                        }
                                        iter.seek(lower.to_be_bytes());
                                        while let Some(tag) = iter.key() {
                                            let tag = tag.try_into().unwrap();
                                            let index = iter.value().unwrap();

                                            solutions.push((
                                                tag,
                                                u64::from_le_bytes(index.try_into().unwrap()),
                                            ));
                                            iter.next();
                                        }
                                    } else {
                                        iter.seek(lower.to_be_bytes());
                                        while let Some(tag) = iter.key() {
                                            let tag = tag.try_into().unwrap();
                                            let index = iter.value().unwrap();
                                            if u64::from_be_bytes(tag) <= upper {
                                                solutions.push((
                                                    tag,
                                                    u64::from_le_bytes(index.try_into().unwrap()),
                                                ));
                                                iter.next();
                                            } else {
                                                break;
                                            }
                                        }
                                    }

                                    solutions
                                });

                                let _ = result_sender.send(Ok(solutions_fut
                                    .await
                                    .unwrap()
                                    .into_iter()
                                    .next()));
                            }
                        }
                    }

                    let write_request = write_requests_receiver.try_next();
                    if write_request.is_ok() {
                        did_nothing = false;
                    }
                    // Process at most write request since reading is higher priority
                    match write_request {
                        Ok(Some(WriteRequests::WriteEncodings {
                            encodings,
                            first_index,
                            result_sender,
                        })) => {
                            let _ = result_sender.send(
                                try {
                                    plot_file
                                        .seek(SeekFrom::Start(first_index * PIECE_SIZE as u64))
                                        .await?;
                                    {
                                        let mut whole_encoding = Vec::with_capacity(
                                            encodings[0].len() * encodings.len(),
                                        );
                                        for encoding in &encodings {
                                            whole_encoding.extend_from_slice(encoding);
                                        }
                                        plot_file.write_all(&whole_encoding).await?;
                                        piece_count.fetch_max(
                                            first_index + encodings.len() as u64,
                                            Ordering::AcqRel,
                                        );
                                    }
                                },
                            );
                        }
                        Ok(Some(WriteRequests::WriteTags {
                            first_index,
                            tags,
                            salt,
                            result_sender,
                        })) => {
                            let _ = result_sender.send(
                                try {
                                    let tags_db = match commitments.get_or_create_db(salt).await {
                                        Ok(tags_db) => tags_db,
                                        Err(error) => {
                                            error!("Failed to open tags database: {}", error);
                                            continue;
                                        }
                                    };
                                    // TODO: remove unwrap
                                    tokio::task::spawn_blocking(move || {
                                        for (tag, index) in tags.iter().zip(first_index..) {
                                            tags_db.put(tag, index.to_le_bytes())?;
                                        }

                                        Ok::<(), rocksdb::Error>(())
                                    })
                                    .await
                                    .unwrap()
                                    .unwrap();
                                },
                            );
                        }
                        Ok(Some(WriteRequests::FinishCommitmentCreation {
                            salt,
                            result_sender,
                        })) => {
                            if let Err(error) = commitments.finish_commitment_creation(salt).await {
                                error!("Failed to finish commitment creation: {}", error);
                                continue;
                            }

                            let _ = result_sender.send(());
                        }
                        Ok(Some(WriteRequests::RemoveCommitment {
                            salt,
                            result_sender,
                        })) => {
                            if let Err(error) = commitments.remove_commitment(salt).await {
                                error!("Failed to remove commitment: {}", error);
                                continue;
                            }

                            let _ = result_sender.send(());
                        }
                        Ok(None) => {
                            break 'outer;
                        }
                        Err(_) => {
                            // Ignore
                        }
                    }
                }

                if let Err(error) = plot_file.sync_all().await {
                    error!("Failed to sync plot file before exit: {}", error);
                }

                commitments
            }
        });

        let inner = Inner {
            background_handle: Some(background_handle),
            any_requests_sender: Some(any_requests_sender),
            read_requests_sender: Some(read_requests_sender),
            write_requests_sender: Some(write_requests_sender),
            plot_db: Some(Arc::new(plot_db)),
            piece_count,
            commitment_statuses: Mutex::new(commitment_statuses),
        };

        Ok(Plot {
            inner: Arc::new(inner),
        })
    }

    /// Whether plot doesn't have anything in it
    pub(crate) async fn is_empty(&self) -> bool {
        self.inner.piece_count.load(Ordering::Acquire) == 0
    }

    /// Reads a piece from plot by index
    pub(crate) async fn read(&self, index: u64) -> io::Result<Piece> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.inner
            .read_requests_sender
            .clone()
            .unwrap()
            .send(ReadRequests::ReadEncoding {
                index,
                result_sender,
            })
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed sending read encoding request: {}", error),
                )
            })?;

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

        result_receiver.await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Read encoding result sender was dropped: {}", error),
            )
        })?
    }

    /// Find pieces within specified solution range.
    ///
    /// Returns tag and piece index.
    pub(crate) async fn find_by_range(
        &self,
        target: [u8; 8],
        range: u64,
        salt: Salt,
    ) -> io::Result<Option<(Tag, u64)>> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.inner
            .read_requests_sender
            .clone()
            .unwrap()
            .send(ReadRequests::FindByRange {
                target,
                range,
                salt,
                result_sender,
            })
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed sending get by range request: {}", error),
                )
            })?;

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

        result_receiver.await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Get by range result sender was dropped: {}", error),
            )
        })?
    }

    // TODO: This should also update commitment for every piece written
    /// Writes a piece to the plot by index, will overwrite if piece exists (updates)
    pub(crate) async fn write_many(
        &self,
        encodings: Vec<Piece>,
        first_index: u64,
    ) -> io::Result<()> {
        if encodings.is_empty() {
            return Ok(());
        }
        let (result_sender, result_receiver) = oneshot::channel();

        self.inner
            .write_requests_sender
            .clone()
            .unwrap()
            .send(WriteRequests::WriteEncodings {
                encodings,
                first_index,
                result_sender,
            })
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed sending write many request: {}", error),
                )
            })?;

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

        result_receiver.await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Write many result sender was dropped: {}", error),
            )
        })?
    }

    // Remove all commitments for all salts except those in the list
    pub(crate) async fn retain_commitments(&self, salts: Vec<Salt>) -> io::Result<()> {
        let salts: Vec<Salt> = self
            .inner
            .commitment_statuses
            .lock()
            .unwrap()
            .drain_filter(|salt, _status| !salts.contains(salt))
            .map(|(salt, _status)| salt)
            .collect();

        for salt in salts {
            self.remove_commitment(salt).await?;
        }

        Ok(())
    }

    pub(crate) async fn create_commitment(&self, salt: Salt) -> io::Result<()> {
        {
            let mut commitment_statuses = self.inner.commitment_statuses.lock().unwrap();
            if let Some(CommitmentStatus::Created) = commitment_statuses.get(&salt) {
                return Ok(());
            }
            commitment_statuses.insert(salt, CommitmentStatus::InProgress);
        }
        let piece_count = self.inner.piece_count.load(Ordering::Acquire);
        for batch_start in (0..piece_count).step_by(BATCH_SIZE as usize) {
            if let Some(CommitmentStatus::Aborted) =
                self.inner.commitment_statuses.lock().unwrap().get(&salt)
            {
                break;
            }
            let pieces_to_process = (batch_start + BATCH_SIZE).min(piece_count) - batch_start;
            let pieces = self.read_pieces(batch_start, pieces_to_process).await?;

            let tags: Vec<Tag> = tokio::task::spawn_blocking(move || {
                pieces
                    .par_chunks_exact(PIECE_SIZE)
                    .map(|piece| crypto::create_tag(piece, &salt))
                    .collect()
            })
            .await
            .unwrap();

            let (result_sender, result_receiver) = oneshot::channel();

            self.inner
                .write_requests_sender
                .clone()
                .unwrap()
                .send(WriteRequests::WriteTags {
                    first_index: batch_start,
                    tags,
                    salt,
                    result_sender,
                })
                .await
                .map_err(|error| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        format!("Failed sending write tags request: {}", error),
                    )
                })?;

            // If fails - it is either full or disconnected, we don't care either way, so ignore result
            let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

            result_receiver.await.map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Write tags result sender was dropped: {}", error),
                )
            })??;
        }

        let aborted = {
            let mut commitment_statuses = self.inner.commitment_statuses.lock().unwrap();
            if let Some(CommitmentStatus::Aborted) = commitment_statuses.get(&salt) {
                commitment_statuses.remove(&salt);
                true
            } else {
                false
            }
        };

        if aborted {
            self.remove_commitment(salt).await?;

            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Commitment creation was aborted",
            ));
        }

        let (result_sender, result_receiver) = oneshot::channel();

        self.inner
            .write_requests_sender
            .clone()
            .unwrap()
            .send(WriteRequests::FinishCommitmentCreation {
                salt,
                result_sender,
            })
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!(
                        "Failed sending finish commitment creation request: {}",
                        error
                    ),
                )
            })?;

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

        result_receiver.await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Finish commitment creation result sender was dropped: {}",
                    error
                ),
            )
        })?;

        let aborted = {
            let mut commitment_statuses = self.inner.commitment_statuses.lock().unwrap();
            if let Some(CommitmentStatus::Aborted) = commitment_statuses.get(&salt) {
                commitment_statuses.remove(&salt);
                true
            } else {
                commitment_statuses.insert(salt, CommitmentStatus::Created);
                false
            }
        };

        if aborted {
            self.remove_commitment(salt).await?;

            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Commitment creation was aborted",
            ));
        }

        Ok(())
    }

    pub(crate) async fn remove_commitment(&self, salt: Salt) -> io::Result<()> {
        {
            let mut commitment_statuses = self.inner.commitment_statuses.lock().unwrap();
            if let Entry::Occupied(mut entry) = commitment_statuses.entry(salt) {
                if matches!(
                    entry.get(),
                    CommitmentStatus::InProgress | CommitmentStatus::Aborted
                ) {
                    entry.insert(CommitmentStatus::Aborted);
                    // In practice deletion will be delayed and will happen from in progress process of
                    // committing when it can be stopped
                    return Ok(());
                }

                entry.remove_entry();
            }
        }

        let (result_sender, result_receiver) = oneshot::channel();

        self.inner
            .write_requests_sender
            .clone()
            .unwrap()
            .send(WriteRequests::RemoveCommitment {
                salt,
                result_sender,
            })
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed sending remove tags request: {}", error),
                )
            })?;

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

        result_receiver.await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Remove tags result sender was dropped: {}", error),
            )
        })
    }

    /// Get last root block
    pub(crate) async fn get_last_root_block(&self) -> Result<Option<RootBlock>, rocksdb::Error> {
        let db = Arc::clone(self.inner.plot_db.as_ref().unwrap());
        tokio::task::spawn_blocking(move || {
            db.get(LAST_ROOT_BLOCK_KEY).map(|maybe_last_root_block| {
                maybe_last_root_block.as_ref().map(|last_root_block| {
                    serde_json::from_slice(last_root_block)
                        .expect("Database contains incorrect last root block")
                })
            })
        })
        .await
        .unwrap()
    }

    /// Store last root block
    pub(crate) async fn set_last_root_block(
        &self,
        last_root_block: &RootBlock,
    ) -> Result<(), rocksdb::Error> {
        let db = Arc::clone(self.inner.plot_db.as_ref().unwrap());
        let last_root_block = serde_json::to_vec(&last_root_block).unwrap();
        tokio::task::spawn_blocking(move || db.put(LAST_ROOT_BLOCK_KEY, last_root_block))
            .await
            .unwrap()
    }

    pub(crate) fn downgrade(&self) -> WeakPlot {
        WeakPlot {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Returns pieces packed one after another in contiguous `Vec<u8>`
    async fn read_pieces(&self, first_index: u64, count: u64) -> io::Result<Vec<u8>> {
        let (result_sender, result_receiver) = oneshot::channel();

        self.inner
            .read_requests_sender
            .clone()
            .unwrap()
            .send(ReadRequests::ReadEncodings {
                first_index,
                count,
                result_sender,
            })
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::Other,
                    format!("Failed sending read encodings request: {}", error),
                )
            })?;

        // If fails - it is either full or disconnected, we don't care either way, so ignore result
        let _ = self.inner.any_requests_sender.clone().unwrap().try_send(());

        result_receiver.await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Read encodings result sender was dropped: {}", error),
            )
        })?
    }
}

#[derive(Clone)]
pub(crate) struct WeakPlot {
    inner: Weak<Inner>,
}

impl WeakPlot {
    pub(crate) fn upgrade(&self) -> Option<Plot> {
        self.inner.upgrade().map(|inner| Plot { inner })
    }
}
