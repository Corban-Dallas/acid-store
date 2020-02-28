/*
 * Copyright 2019-2020 Wren Powell
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::borrow::{Borrow, ToOwned};
use std::collections::HashSet;
use std::fmt::Debug;
use std::hash::Hash;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

use rmp_serde::{from_read, to_vec};
use uuid::Uuid;

use lazy_static::lazy_static;

use crate::store::DataStore;

use super::chunk_store::ChunkStore;
use super::config::RepositoryConfig;
use super::encryption::{Encryption, EncryptionKey, KeySalt};
use super::header::{Header, Key};
use super::lock::{LockStrategy, LockTable};
use super::metadata::{RepositoryInfo, RepositoryMetadata, RepositoryStats};
use super::object::{chunk_hash, Object, ObjectHandle};
use super::state::RepositoryState;

lazy_static! {
    /// The block ID of the block which stores unencrypted metadata for the repository.
    static ref METADATA_BLOCK_ID: Uuid =
        Uuid::parse_str("8691d360-29c6-11ea-8bc1-2fc8cfe66f33").unwrap();

    /// The block ID of the block which stores the repository format version.
    static ref VERSION_BLOCK_ID: Uuid =
        Uuid::parse_str("cbf28b1c-3550-11ea-8cb0-87d7a14efe10").unwrap();

    /// The current repository format version ID.
    ///
    /// This must be changed any time a backwards-incompatible change is made to the repository
    /// format.
    static ref VERSION_ID: Uuid =
        Uuid::parse_str("036e2a8e-4b53-11ea-a6e9-57c2a822fccf").unwrap();

    /// A table of locks on repositories.
    static ref REPO_LOCKS: RwLock<LockTable> = RwLock::new(LockTable::new());
}

/// A persistent object store.
///
/// An `ObjectRepository` maps keys of type `K` to seekable binary blobs called objects and stores
/// them persistently in a `DataStore`.
///
/// Data in a repository is transparently deduplicated using content-defined block-level
/// deduplication via the ZPAQ chunking algorithm. The data and metadata in the repository can
/// optionally be compressed and encrypted.
///
/// A repository cannot be open more than once simultaneously. Once it is opened, it is locked from
/// further open attempts until the `ObjectRepository` is dropped. This lock prevents the repository
/// from being opened from other threads and processes, but not from other machines.
///
/// Changes made to a repository are not persisted to the data store until `commit` is called. When
/// the `ObjectRepository` is dropped, any uncommitted changes are rolled back automatically.
///
/// # Encryption
/// If encryption is enabled, the Argon2id key derivation function is used to derive a key from a
/// user-supplied password. This key is used to encrypt the repository's randomly generated master
/// key, which is used to encrypt all data in the repository. This setup means that the repository's
/// password can be changed without re-encrypting any data.
///
/// The master key is generated using the operating system's secure random number generator. Both
/// the master key and the derived key are zeroed in memory once they go out of scope.
///
/// Data in a data store is identified by UUIDs and not hashes, so data hashes are not leaked. The
/// repository does not attempt to hide the size of chunks produced by the chunking algorithm, but
/// information about which chunks belong to which objects is encrypted.
///
/// The information in `RepositoryInfo` is never encrypted.
#[derive(Debug)]
pub struct ObjectRepository<K: Key, S: DataStore> {
    /// The state for this object repository.
    state: RwLock<RepositoryState<K, S>>,
}

impl<K: Key, S: DataStore> ObjectRepository<K, S> {
    /// Borrow this repository's state immutably.
    ///
    /// The purpose of this method is to enforce safe usage of the `RwLock` using references.
    fn borrow_state(&self) -> RwLockReadGuard<RepositoryState<K, S>> {
        self.state.read().unwrap()
    }

    /// Borrow this repository's state mutably.
    ///
    /// The purpose of this method is to enforce safe usage of the `RwLock` using references.
    fn borrow_state_mut(&mut self) -> RwLockWriteGuard<RepositoryState<K, S>> {
        self.state.write().unwrap()
    }

    /// Get a `ChunkStore` for this repository.
    ///
    /// The purpose of this method is to enforce safe usage of the `RwLock` using references.
    fn chunk_store(&mut self) -> ChunkStore<K, S> {
        ChunkStore::new(&self.state)
    }

    /// Create a new repository backed by the given data `store`.
    ///
    /// A `config` must be provided to configure the new repository. If encryption is enabled, a
    /// `password` must be provided; otherwise, this argument can be `None`.
    ///
    /// # Errors
    /// - `Error::AlreadyExists`: A repository already exists in the given `store`.
    /// - `Error::Password` A password was required but not provided or provided but not required.
    /// - `Error::Store`: An error occurred with the data store.
    pub fn create_repo(
        mut store: S,
        config: RepositoryConfig,
        password: Option<&[u8]>,
    ) -> crate::Result<Self> {
        // Return an error if a password was required but not provided.
        if password.is_none() && config.encryption != Encryption::None {
            return Err(crate::Error::Password);
        }

        // Return an error if a password was provided but not required.
        if password.is_some() && config.encryption == Encryption::None {
            return Err(crate::Error::Password);
        }

        // Acquire an exclusive lock on the repository.
        let id = Uuid::new_v4();
        let lock = REPO_LOCKS
            .write()
            .unwrap()
            .acquire_lock(id, LockStrategy::Abort)
            .map_err(|_| crate::Error::AlreadyExists)?;

        // Check if the repository already exists.
        if store
            .read_block(*VERSION_BLOCK_ID)
            .map_err(anyhow::Error::from)?
            .is_some()
        {
            return Err(crate::Error::AlreadyExists);
        }

        // Generate the master encryption key.
        let master_key = match password {
            Some(..) => EncryptionKey::generate(config.encryption.key_size()),
            None => EncryptionKey::new(Vec::new()),
        };

        // Encrypt the master encryption key.
        let salt = KeySalt::generate();
        let encrypted_master_key = match password {
            Some(password_bytes) => {
                let user_key = EncryptionKey::derive(
                    password_bytes,
                    &salt,
                    config.encryption.key_size(),
                    config.memory_limit.to_mem_limit(),
                    config.operations_limit.to_ops_limit(),
                );
                config.encryption.encrypt(master_key.as_ref(), &user_key)
            }
            None => Vec::new(),
        };

        // Generate and write the header.
        let header = Header::default();
        let serialized_header = to_vec(&header).expect("Could not serialize header.");
        let compressed_header = config.compression.compress(&serialized_header)?;
        let encrypted_header = config.encryption.encrypt(&compressed_header, &master_key);
        let header_id = Uuid::new_v4();
        store
            .write_block(header_id, &encrypted_header)
            .map_err(anyhow::Error::from)?;

        // Create the repository metadata with a reference to the newly-written header.
        let metadata = RepositoryMetadata {
            id,
            chunker_bits: config.chunker_bits,
            compression: config.compression,
            encryption: config.encryption,
            memory_limit: config.memory_limit,
            operations_limit: config.operations_limit,
            master_key: encrypted_master_key,
            salt,
            header: header_id,
            creation_time: SystemTime::now(),
        };

        // Write the repository metadata.
        let serialized_metadata = to_vec(&metadata).expect("Could not serialize metadata.");
        store
            .write_block(*METADATA_BLOCK_ID, &serialized_metadata)
            .map_err(anyhow::Error::from)?;

        // Write the repository version. We do this last because this signifies that the repository
        // is done being created.
        store
            .write_block(*VERSION_BLOCK_ID, VERSION_ID.as_bytes())
            .map_err(anyhow::Error::from)?;

        let state = RwLock::new(RepositoryState {
            store,
            metadata,
            header,
            master_key,
            lock,
        });

        Ok(ObjectRepository { state })
    }

    /// Open the repository in the given data `store`.
    ///
    /// If encryption is enabled, a `password` must be provided. Otherwise, this argument can be
    /// `None`.
    ///
    /// # Errors
    /// - `Error::NotFound`: There is no repository in the given `store`.
    /// - `Error::Corrupt`: The repository is corrupt. This is most likely unrecoverable.
    /// - `Error::Locked`: The repository is locked and `LockStrategy::Abort` was used.
    /// - `Error::Password`: The password provided is invalid.
    /// - `Error::KeyType`: The type `K` does not match the data in the repository.
    /// - `Error::UnsupportedFormat`: This repository format is not supported by this version of
    /// the library.
    /// - `Error::Store`: An error occurred with the data store.
    pub fn open_repo(
        mut store: S,
        password: Option<&[u8]>,
        strategy: LockStrategy,
    ) -> crate::Result<Self> {
        // Acquire a lock on the repository.
        let repository_id = Self::peek_info(&mut store)?.id();
        let lock = REPO_LOCKS
            .write()
            .unwrap()
            .acquire_lock(repository_id, strategy)?;

        // Read the repository version to see if this is a compatible repository.
        let serialized_version = store
            .read_block(*VERSION_BLOCK_ID)
            .map_err(anyhow::Error::from)?
            .ok_or(crate::Error::NotFound)?;
        let version =
            Uuid::from_slice(serialized_version.as_slice()).map_err(|_| crate::Error::Corrupt)?;
        if version != *VERSION_ID {
            return Err(crate::Error::UnsupportedFormat);
        }

        // We read the metadata again after reading the UUID to prevent a race condition when
        // acquiring the lock.
        let serialized_metadata = store
            .read_block(*METADATA_BLOCK_ID)
            .map_err(anyhow::Error::from)?
            .ok_or(crate::Error::Corrupt)?;
        let metadata: RepositoryMetadata =
            from_read(serialized_metadata.as_slice()).map_err(|_| crate::Error::Corrupt)?;

        // Decrypt the master key for the repository.
        let master_key = match password {
            Some(password_bytes) => {
                let user_key = EncryptionKey::derive(
                    password_bytes,
                    &metadata.salt,
                    metadata.encryption.key_size(),
                    metadata.memory_limit.to_mem_limit(),
                    metadata.operations_limit.to_ops_limit(),
                );
                EncryptionKey::new(
                    metadata
                        .encryption
                        .decrypt(&metadata.master_key, &user_key)
                        .map_err(|_| crate::Error::Password)?,
                )
            }
            None => EncryptionKey::new(Vec::new()),
        };

        // Read, decrypt, decompress, and deserialize the header.
        let encrypted_header = store
            .read_block(metadata.header)
            .map_err(anyhow::Error::from)?
            .ok_or(crate::Error::Corrupt)?;
        let compressed_header = metadata
            .encryption
            .decrypt(&encrypted_header, &master_key)
            .map_err(|_| crate::Error::Corrupt)?;
        let serialized_header = metadata
            .compression
            .decompress(&compressed_header)
            .map_err(|_| crate::Error::Corrupt)?;
        let header: Header<K> =
            from_read(serialized_header.as_slice()).map_err(|_| crate::Error::KeyType)?;

        let state = RwLock::new(RepositoryState {
            store,
            metadata,
            header,
            master_key,
            lock,
        });

        Ok(ObjectRepository { state })
    }

    /// Return whether the given `key` exists in this repository.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let state = self.borrow_state();
        state.header.objects.contains_key(key)
    }

    /// Insert the given `key` into the repository and return a new object.
    ///
    /// If the given `key` already exists in the repository, its object is replaced. The returned
    /// object represents the data associated with the `key`.
    pub fn insert(&mut self, key: K) -> Object<K, S> {
        let mut state = self.borrow_state_mut();
        state
            .header
            .objects
            .insert(key.clone(), ObjectHandle::default());
        state.header.clean_chunks();

        drop(state);
        Object::new(&self.state, key)
    }

    /// Remove the object associated with `key` from the repository.
    ///
    /// This returns `true` if the object was removed or `false` if it didn't exist.
    ///
    /// The space used by the given object isn't freed and made available for new objects until
    /// `commit` is called.
    pub fn remove<Q>(&mut self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let mut state = self.borrow_state_mut();
        let handle = state.header.objects.remove(key);
        state.header.clean_chunks();
        handle.is_some()
    }

    /// Return the object associated with `key` or `None` if it doesn't exist.
    pub fn get<Q>(&self, key: &Q) -> Option<Object<K, S>>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ToOwned<Owned = K> + ?Sized,
    {
        let state = self.borrow_state();
        state.header.objects.get(&key)?;
        Some(Object::new(&self.state, key.to_owned()))
    }

    /// Return an iterator over all the keys in this repository.
    pub fn keys<'a>(&'a mut self) -> impl Iterator<Item = &'a K> + 'a {
        self.state.get_mut().unwrap().header.objects.keys()
    }

    /// Copy the object at `source` to `dest`.
    ///
    /// This is a cheap operation which does not require copying the bytes in the object.
    ///
    /// # Errors
    /// - `Error::NotFound`: There is no object at `source`.
    /// - `Error::AlreadyExists`: There is already an object at `dest`.
    pub fn copy<Q>(&mut self, source: &Q, dest: K) -> crate::Result<()>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.contains(dest.borrow()) {
            return Err(crate::Error::AlreadyExists);
        }

        let mut state = self.borrow_state_mut();

        let source_object = state
            .header
            .objects
            .get(source)
            .ok_or(crate::Error::NotFound)?
            .clone();

        state.header.objects.insert(dest, source_object);

        Ok(())
    }

    /// Return a list of blocks in `store` excluding those used to store metadata.
    fn list_data_blocks(&mut self) -> crate::Result<Vec<Uuid>> {
        let mut state = self.borrow_state_mut();

        let all_blocks = state.store.list_blocks().map_err(anyhow::Error::from)?;

        Ok(all_blocks
            .iter()
            .copied()
            .filter(|id| {
                *id != *METADATA_BLOCK_ID
                    && *id != *VERSION_BLOCK_ID
                    && *id != state.metadata.header
            })
            .collect())
    }

    /// Commit changes which have been made to the repository.
    ///
    /// No changes are saved persistently until this method is called. Committing a repository is an
    /// atomic and consistent operation; changes cannot be partially committed and interrupting a
    /// commit will never leave the repository in an inconsistent state.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn commit(&mut self) -> crate::Result<()> {
        let state = self.borrow_state();

        // Serialize and encode the header.
        let serialized_header = to_vec(&state.header).expect("Could not serialize header.");
        drop(state);
        let encoded_header = self.chunk_store().encode_data(&serialized_header)?;

        let mut state = self.borrow_state_mut();

        // Write the new header to the data store.
        let header_id = Uuid::new_v4();
        state
            .store
            .write_block(header_id, &encoded_header)
            .map_err(anyhow::Error::from)?;
        state.metadata.header = header_id;

        // Write the repository metadata, atomically completing the commit.
        let serialized_metadata = to_vec(&state.metadata).expect("Could not serialize metadata.");
        state
            .store
            .write_block(*METADATA_BLOCK_ID, &serialized_metadata)
            .map_err(anyhow::Error::from)?;

        // After changes are committed, remove any unused chunks from the data store.
        let referenced_chunks = state
            .header
            .chunks
            .values()
            .copied()
            .collect::<HashSet<_>>();

        drop(state);
        let data_blocks = self.list_data_blocks()?;
        let mut state = self.borrow_state_mut();

        for stored_chunk in data_blocks {
            if !referenced_chunks.contains(&stored_chunk) {
                state
                    .store
                    .remove_block(stored_chunk)
                    .map_err(anyhow::Error::from)?;
            }
        }

        Ok(())
    }

    /// Verify the integrity of all the data in the repository.
    ///
    /// This returns the set of keys of objects which are corrupt. This is more efficient than
    /// calling `Object::verify` on each object in the repository.
    ///
    /// # Errors
    /// - `Error::InvalidData`: Ciphertext verification failed.
    /// - `Error::Store`: An error occurred with the data store.
    /// - `Error::Io`: An I/O error occurred.
    pub fn verify(&mut self) -> crate::Result<HashSet<&K>> {
        let state = self.borrow_state();

        let mut corrupt_chunks = HashSet::new();
        let expected_chunks = state.header.chunks.keys().copied().collect::<Vec<_>>();
        drop(state);

        // Get the set of hashes of chunks which are corrupt.
        for chunk in expected_chunks {
            match self.chunk_store().read_chunk(chunk) {
                Ok(data) => {
                    if data.len() != chunk.size || chunk_hash(&data) != chunk.hash {
                        corrupt_chunks.insert(chunk.hash);
                    }
                }
                Err(crate::Error::InvalidData) => {
                    // Ciphertext verification failed. No need to check the hash.
                    corrupt_chunks.insert(chunk.hash);
                }
                Err(error) => return Err(error),
            };
        }

        // If there are no corrupt chunks, there are no corrupt objects.
        if corrupt_chunks.is_empty() {
            return Ok(HashSet::new());
        }

        let mut corrupt_objects = HashSet::new();

        for (key, object) in self.state.get_mut().unwrap().header.objects.iter() {
            for chunk in &object.chunks {
                // If any one of the object's chunks is corrupt, the object is corrupt.
                if corrupt_chunks.contains(&chunk.hash) {
                    corrupt_objects.insert(key);
                    break;
                }
            }
        }

        Ok(corrupt_objects)
    }

    /// Change the password for this repository.
    ///
    /// This replaces the existing password with `new_password`. Changing the password does not
    /// require re-encrypting any data. The change does not take effect until `commit` is called.
    /// If encryption is disabled, this method does nothing.
    pub fn change_password(&mut self, new_password: &[u8]) {
        let mut state = self.borrow_state_mut();

        let salt = KeySalt::generate();
        let user_key = EncryptionKey::derive(
            new_password,
            &salt,
            state.metadata.encryption.key_size(),
            state.metadata.memory_limit.to_mem_limit(),
            state.metadata.operations_limit.to_ops_limit(),
        );

        let encrypted_master_key = state
            .metadata
            .encryption
            .encrypt(state.master_key.as_ref(), &user_key);

        state.metadata.salt = salt;
        state.metadata.master_key = encrypted_master_key;
    }

    /// Return information about the repository.
    pub fn info(&self) -> RepositoryInfo {
        let state = self.borrow_state();
        state.metadata.to_info()
    }

    /// Return information about the repository in `store` without opening it.
    ///
    /// # Errors
    /// - `Error::NotFound`: There is no repository in the given `store`.
    /// - `Error::Corrupt`: The repository is corrupt. This is most likely unrecoverable.
    /// - `Error::Store`: An error occurred with the data store.
    pub fn peek_info(store: &mut S) -> crate::Result<RepositoryInfo> {
        // Read and deserialize the metadata.
        let serialized_metadata = match store
            .read_block(*METADATA_BLOCK_ID)
            .map_err(anyhow::Error::from)?
        {
            Some(data) => data,
            None => return Err(crate::Error::NotFound),
        };
        let metadata: RepositoryMetadata =
            from_read(serialized_metadata.as_slice()).map_err(|_| crate::Error::Corrupt)?;

        Ok(metadata.to_info())
    }

    /// Calculate statistics about the repository.
    pub fn stats(&self) -> RepositoryStats {
        let state = self.borrow_state();

        let apparent_size = state
            .header
            .objects
            .values()
            .map(|object| object.size)
            .sum();
        let actual_size = state
            .header
            .chunks
            .keys()
            .map(|chunk| chunk.size as u64)
            .sum();

        RepositoryStats {
            apparent_size,
            actual_size,
        }
    }

    /// Consume this repository and return the wrapped `DataStore`.
    pub fn into_store(self) -> S {
        self.state.into_inner().unwrap().store
    }
}
