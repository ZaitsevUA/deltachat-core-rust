//! Transfer a backup to an other device.
//!
//! This module provides support for using n0's sendme tool to initiate transfer of a backup
//! to another device using a QR code.
//!
//! Using the sendme terminology there are two parties to this:
//!
//! - The *Provider*, which starts a server and listens for connections.
//! - The *Getter*, which connects to the server and retrieves the data.
//!
//! Sendme is designed around the idea of verifying hashes, the downloads are verified as
//! they are retrieved.  The entire transfer is initiated by requesting the data of a single
//! root hash.
//!
//! Both the provider and the getter are authenticated:
//!
//! - The provider is known by its *peer ID*.
//! - The provider needs an *authentication token* from the getter before it accepts a
//!   connection.
//!
//! Both these are transferred in the QR code offered to the getter.  This ensures that the
//! getter can not connect to an impersonated provider and the provider does not offer the
//! download to an impersonated getter.

use std::path::Path;

use crate::chat::delete_and_reset_all_device_msgs;
use crate::context::Context;
use crate::e2ee;
use crate::qr::Qr;
use anyhow::{anyhow, bail, ensure, format_err, Context as _, Result};
use async_channel::Receiver;
use futures_lite::StreamExt;
use sendme::blobs::Collection;
use sendme::get::{DataStream, Options};
use sendme::protocol::AuthToken;
use sendme::provider::{DataSource, Event, Provider, Ticket};
use sendme::Hash;
use tokio::fs::{self, File};
use tokio::io::{self, BufWriter};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReadDirStream;

use super::{export_database, BlobDirContents, DeleteOnDrop, DBFILE_BACKUP_NAME};

/// Provide or send a backup of this device.
///
/// This creates a backup of the current device and starts a service which offers another
/// device to download this backup.
///
/// This does not make a full backup on disk, only the SQLite database is created on disk,
/// the blobs in the blob directory are not copied.
///
/// This starts a task which acquires the global "ongoing" mutex.  If you need to stop the
/// task use the [`Context::stop_ongoing`] mechanism.
#[derive(Debug)]
pub struct BackupProvider {
    /// The supervisor task, run by [`BackupProvider::watch_provider`].
    handle: JoinHandle<Result<()>>,
    /// The ticket to retrieve the backup collection.
    ticket: Ticket,
}

impl BackupProvider {
    /// Prepares for sending a backup to a second device.
    ///
    /// Before calling this function all I/O must be stopped so that no changes to the blobs
    /// or database are happening, this is done by calling the `dc_accounts_stop_io` or
    /// `dc_stop_io` APIs first.  TODO: Add the rust equivalents.
    ///
    /// This will acquire the global "ongoing process" mutex.  You must call
    /// [`BackupSender::join`] after creating this struct, otherwise this will not respect
    /// the possible cancellation of the "ongoing process".
    pub async fn perpare(context: &Context, dir: &Path) -> Result<Self> {
        ensure!(
            // TODO: Should we worry about path normalisation?
            dir != context.get_blobdir(),
            "Temporary database export directory should not be in blobdir"
        );
        e2ee::ensure_secret_key_exists(context)
            .await
            .context("Private key not available, aborting backup export")?;

        // Acquire global "ongoing" mutex.
        let cancel_token = context.alloc_ongoing().await?;
        let res = tokio::select! {
            biased;
            res = Self::prepare_inner(context, dir) => {
                match res {
                    Ok(slf) => {
                        // TODO: maybe this is the wrong place to log this
                        // TODO: Also needs to log progress somehow.
                        info!(context, "Waiting for remote to connect");
                        Ok(slf)
                    },
                    Err(err) => {
                        error!(context, "Failed to set up second device setup: {:#}", err);
                        Err(err)
                    },
                }
            },
            _ = cancel_token.recv() => Err(format_err!("cancelled")),
        };
        let (provider, ticket) = match res {
            Ok((provider, ticket)) => (provider, ticket),
            Err(err) => {
                context.free_ongoing().await;
                return Err(err);
            }
        };
        let handle = tokio::spawn(Self::watch_provider(provider, cancel_token));
        Ok(Self { handle, ticket })
    }

    /// Creates the provider and supervisor tasks.
    ///
    /// Having this as a function makes it easier to cancel it when needed.
    async fn prepare_inner(context: &Context, dir: &Path) -> Result<(Provider, Ticket)> {
        // Generate the token up front: we also use it to encrypt the database.
        let token = AuthToken::generate();
        let dbfile = dir.join(DBFILE_BACKUP_NAME);
        export_database(context, &dbfile, token.to_string())
            .await
            .context("Database export failed")?;

        // Now we can be sure IO is not running.
        let mut files = vec![DataSource::from(dbfile)];
        let blobdir = BlobDirContents::new(context).await?;
        for blob in blobdir.iter() {
            files.push(blob.to_abs_path().into());
        }

        // Start listening.
        let (db, hash) = sendme::provider::create_collection(files).await?;
        let provider = Provider::builder(db).auth_token(token).spawn()?;
        let ticket = provider.ticket(hash);
        Ok((provider, ticket))
    }

    /// Supervises the sendme [`Provider`] terminating it when needed.
    ///
    /// This will watch the provider and terminate it when:
    /// - A transfer is completed, successful or unsuccessful.
    /// - An event could not be observed to protect against not knowing of a completed event.
    /// - The ongoing process is cancelled.
    ///
    /// The *cancel_token* is the handle for the ongoing process mutex, when this completes
    /// we must cancel this operation.
    async fn watch_provider(mut provider: Provider, cancel_token: Receiver<()>) -> Result<()> {
        let mut events = provider.subscribe();
        loop {
            tokio::select! {
                biased;
                res = &mut provider => {
                    return res.context("BackupSender failed");
                },
                maybe_event = events.recv() => {
                    match maybe_event {
                        Ok(event) => {
                            match event {
                                Event::TransferCompleted { .. } => {
                                    provider.abort();
                                }
                                Event::TransferAborted { .. } => {
                                    provider.abort();
                                    return Err(anyhow!("BackupSender transfer aborted"));
                                }
                                _ => (),
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // We should never see this, provider.join() should complete
                            // first.
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // We really shouldn't be lagging, if we did we may have missed
                            // a completion event.
                            provider.abort();
                            return Err(anyhow!("Missed events from BackupSender"));
                        }
                    }
                },
                _ = cancel_token.recv() => {
                    provider.abort();
                    return Err(anyhow!("BackupSender cancelled"));
                },
            }
        }
    }

    pub fn qr(&self) -> Qr {
        Qr::Backup {
            ticket: self.ticket.clone(),
        }
    }
}

/// Contacts a backup provider and receives the backup from it.
///
/// This uses a QR code to contact another instance of deltachat which is providing a backup
/// using the [`BackupProvider`].  Once connected it will authenticate using the secrets in
/// the QR code and retrieve the backup.
///
/// Using [`Qr`] as argument is a bit odd as it only accepts one specific variant of it.  It
/// does avoid having [`sendme::provider::Ticket`] in the primary API however, without
/// having to revert to untyped bytes.
pub async fn get_backup(context: &Context, qr: Qr) -> Result<()> {
    let Qr::Backup { ticket } = qr else {
        bail!("QR code for backup must be of type DCBACKUP");
    };
    ensure!(
        !context.is_configured().await?,
        "Cannot import backups to accounts in use."
    );
    ensure!(
        context.scheduler.read().await.is_none(),
        "cannot import backup, IO is running"
    );
    let opts = Options {
        addr: ticket.addr,
        peer_id: Some(ticket.peer),
    };
    let on_blob = |hash, reader, name| on_blob(context, &ticket, hash, reader, name);
    match sendme::get::run(
        ticket.hash,
        ticket.token,
        opts,
        on_connected,
        on_collection,
        on_blob,
    )
    .await
    {
        Ok(_) => {
            delete_and_reset_all_device_msgs(context).await?;
            Ok(())
        }
        Err(err) => {
            // Clean up any blobs we already wrote.
            let readdir = fs::read_dir(context.get_blobdir()).await?;
            let mut readdir = ReadDirStream::new(readdir);
            while let Some(dirent) = readdir.next().await {
                if let Ok(dirent) = dirent {
                    fs::remove_file(dirent.path()).await.ok();
                }
            }
            Err(err)
        }
    }
}

/// Get callback when the connection is established with the provider.
#[allow(clippy::unused_async)]
async fn on_connected() -> Result<()> {
    Ok(())
}

/// Get callback when a collection is received from the provider.
#[allow(clippy::unused_async)]
async fn on_collection(_collection: Collection) -> Result<()> {
    Ok(())
}

/// Get callback when a blob is received from the provider.
async fn on_blob(
    context: &Context,
    ticket: &Ticket,
    _hash: Hash,
    mut reader: DataStream,
    name: String,
) -> Result<DataStream> {
    ensure!(!name.is_empty(), "Received a nameless blob");
    let path = if name == DBFILE_BACKUP_NAME {
        // We can only safely write to the blobdir.  But the blobdir could have a file named
        // exactly like our special name.  We solve this by using an uppercase extension
        // which is forbidden for normal blobs.
        context.get_blobdir().join(format!("{name}.SPECIAL"))
    } else {
        context.get_blobdir().join(&name)
    };
    let _guard = if name == DBFILE_BACKUP_NAME {
        Some(DeleteOnDrop(path.clone()))
    } else {
        None
    };
    let file = File::create(&path).await?;
    let mut file = BufWriter::with_capacity(128 * 1024, file);
    io::copy(&mut reader, &mut file).await?;
    if name == DBFILE_BACKUP_NAME {
        context
            .sql
            .import(&path, ticket.token.to_string())
            .await
            .context("cannot import database")?;
        fs::remove_file(&path)
            .await
            .with_context(|| format!("database import file: {}", path.display()))?;
    }
    Ok(reader)
}
