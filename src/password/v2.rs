// Copyright 2014 The Rooster Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::super::ffi;
use super::super::crypto::{scrypt, hmac, sha2};
use super::super::crypto::digest::Digest;
use super::super::crypto::mac::{Mac, MacResult};
use super::super::aes;
use super::super::rand::{Rng, OsRng};
use super::super::byteorder::{ReadBytesExt, WriteBytesExt, BigEndian, Error as ByteorderError};
use super::super::rustc_serialize::json;
use super::super::safe_string::SafeString;
use super::super::safe_vec::SafeVec;
use super::PasswordError;
use std::io::{Seek, SeekFrom, Result as IoResult, Error as IoError, ErrorKind as IoErrorKind, Read, Write, Cursor};
use std::fs::File;
use std::ops::DerefMut;
use std::ops::Deref;

/// The schema of the JSON content in the password file.
///
/// ```
/// {
///     "passwords": [
///         "name": "YouTube",
///         "username": "conradk",
///         "password": "xxxxxxxx",
///         "created_at": 23145436,
///         "updated_at": 23145546,
///     ]
/// }
/// ```

/// The IV is 128 bits long.
///
/// This should be enough for it to be unique. Also, the password storage file
/// is normally unique, so if an attacker gets it, having access to the IV
/// doesn't help much, since there is no other data that uses the same IV to
/// compare with.
const IV_LEN: usize = 16;

/// Length of the key derived ffrom the user password.
const KEY_LEN: usize = 32;

/// Length of the salt passed to the key derivation function.
const SALT_LEN: usize = 32;

/// Length of the HMAC signature
const SIGNATURE_LEN: usize = 64;

/// Scrypt parameters
const SCRYPT_PARAM_LOG2_N: u8 = 12;
const SCRYPT_PARAM_R: u32 = 8;
const SCRYPT_PARAM_P: u32 = 1;

/// The version of this lib
const VERSION: u32 = 2;

// Create a random IV.
fn generate_random_iv() -> IoResult<[u8; IV_LEN]> {
    let mut bytes: [u8; IV_LEN] = [0; IV_LEN];
    let mut rng = try!(OsRng::new());
    rng.fill_bytes(&mut bytes);
    Ok(bytes)
}

// Create a random salt.
fn generate_random_salt() -> IoResult<[u8; SALT_LEN]> {
    let mut bytes: [u8; SALT_LEN] = [0; SALT_LEN];
    let mut rng = try!(OsRng::new());
    rng.fill_bytes(&mut bytes);
    Ok(bytes)
}

/// Derives a 256 bits encryption key from the password.
fn generate_encryption_key(scrypt_params: scrypt::ScryptParams, master_password: &str, salt: [u8; SALT_LEN]) -> SafeVec {
    let mut vec = Vec::<u8>::with_capacity(KEY_LEN);
    for _ in 0..KEY_LEN {
        vec.push(0u8);
    }
    let mut output = SafeVec::new(vec);

    scrypt::scrypt(master_password.as_bytes(), &salt, &scrypt_params, output.deref_mut());

    output
}

/// Creates a HMAC struct
fn digest(key: &[u8], version: u32, scrypt_log2_n: u8, scrypt_r: u32, scrypt_p: u32, iv: &[u8], salt: &[u8], blob: &[u8]) -> IoResult<hmac::Hmac<sha2::Sha512>> {
    let mut digest = hmac::Hmac::new(sha2::Sha512::new(), key);

    let mut version_bytes_cursor: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    try!(version_bytes_cursor.write_u32::<BigEndian>(version).map_err(|be| {
        match be {
            ByteorderError::Io(io_err) => io_err,
            ByteorderError::UnexpectedEOF => IoError::new(IoErrorKind::Other, "unexpected eof")
        }
    }));

    let mut scrypt_bytes_cursor: Cursor<Vec<u8>> = Cursor::new(Vec::new());
    try!(scrypt_bytes_cursor.write_u8(scrypt_log2_n).map_err(|be| {
        match be {
            ByteorderError::Io(io_err) => io_err,
            ByteorderError::UnexpectedEOF => IoError::new(IoErrorKind::Other, "unexpected eof")
        }
    }));
    try!(scrypt_bytes_cursor.write_u32::<BigEndian>(scrypt_r).map_err(|be| {
        match be {
            ByteorderError::Io(io_err) => io_err,
            ByteorderError::UnexpectedEOF => IoError::new(IoErrorKind::Other, "unexpected eof")
        }
    }));
    try!(scrypt_bytes_cursor.write_u32::<BigEndian>(scrypt_p).map_err(|be| {
        match be {
            ByteorderError::Io(io_err) => io_err,
            ByteorderError::UnexpectedEOF => IoError::new(IoErrorKind::Other, "unexpected eof")
        }
    }));

    let version_bytes = version_bytes_cursor.into_inner();
    digest.input(version_bytes.deref());

    let scrypt_bytes = scrypt_bytes_cursor.into_inner();
    digest.input(scrypt_bytes.deref());

    digest.input(&iv);
    digest.input(&salt);
    digest.input(blob.deref());

    Ok(digest)
}


/// The format of the encrypted JSON content in the password file v1.
#[derive(RustcDecodable, RustcEncodable, Clone)]
pub struct Schema {
    passwords: Vec<Password>,
}

impl Schema {
    fn new() -> Schema {
        Schema {
            passwords: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, RustcDecodable, RustcEncodable)]
pub struct Password {
    pub name: String,
    pub username: String,
    pub password: SafeString,
    pub created_at: ffi::time_t,
    pub updated_at: ffi::time_t
}

impl Password {
    pub fn new(name: String, username: String, password: SafeString) -> Password {
        let timestamp = ffi::time();
        Password {
            name: name,
            username: username,
            password: password,
            created_at: timestamp,
            updated_at: timestamp
        }
    }
}

pub struct PasswordStore {
    key: SafeVec,
    scrypt_log2_n: u8,
    scrypt_r: u32,
    scrypt_p: u32,
    salt: [u8; SALT_LEN],
    schema: Schema,
}

/// Read and writes to a Rooster file
///
/// The Rooster file has the following format:
/// - rooster version: u32, big endian
/// - scrypt log2_n:  u8
/// - scrypt r:       u32, big endian
/// - scrypt p:       u32, big endian
/// - salt:           256 bits
/// - iv:             256 bits
/// - signature:      512 bits HMAC-SHA512
/// - encrypted blob: variable length
impl PasswordStore {
    pub fn new(master_password: SafeString) -> IoResult<PasswordStore> {
        let salt = try!(generate_random_salt());

        let scrypt_params = scrypt::ScryptParams::new(
            SCRYPT_PARAM_LOG2_N,
            SCRYPT_PARAM_R,
            SCRYPT_PARAM_P
        );

        let key = generate_encryption_key(scrypt_params.clone(), master_password.deref(), salt);

        Ok(PasswordStore {
            key: key,
            scrypt_log2_n: SCRYPT_PARAM_LOG2_N,
            scrypt_r: SCRYPT_PARAM_R,
            scrypt_p: SCRYPT_PARAM_P,
            salt: salt,
            schema: Schema::new(),
        })
    }

    pub fn from_input(master_password: SafeString, input: SafeVec) -> Result<PasswordStore, PasswordError> {
        let mut reader = Cursor::new(input.deref());

        // Version taken from network byte order (big endian).
        let version = match reader.read_u32::<BigEndian>() {
            Ok(version) => version,
            Err(err) => {
                let err = match err {
                   ByteorderError::UnexpectedEOF => PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")),
                   ByteorderError::Io(io_err) => PasswordError::Io(io_err)
                };
                return Err(err);
            }
        };
        if version != VERSION {
            return Err(PasswordError::WrongVersionError);
        }

        // Read the scrypt params.
        let scrypt_log2_n = match reader.read_u8() {
            Ok(n) => n,
            Err(err) => {
                let err = match err {
                   ByteorderError::UnexpectedEOF => PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")),
                   ByteorderError::Io(io_err) => PasswordError::Io(io_err)
                };
                return Err(err);
            }
        };
        let scrypt_r = match reader.read_u32::<BigEndian>() {
            Ok(n) => n,
            Err(err) => {
                let err = match err {
                   ByteorderError::UnexpectedEOF => PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")),
                   ByteorderError::Io(io_err) => PasswordError::Io(io_err)
                };
                return Err(err);
            }
        };
        let scrypt_p = match reader.read_u32::<BigEndian>() {
            Ok(n) => n,
            Err(err) => {
                let err = match err {
                   ByteorderError::UnexpectedEOF => PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")),
                   ByteorderError::Io(io_err) => PasswordError::Io(io_err)
                };
                return Err(err);
            }
        };

        // Read the old salt.
        let mut salt: [u8; SALT_LEN] = [0u8; SALT_LEN];
        try!(reader.read(&mut salt).map_err(|io_err| PasswordError::Io(io_err)).and_then(|num_bytes| {
            if num_bytes == SALT_LEN {
                Ok(())
            } else {
                Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")))
            }
        }));

        // Read the old IV.
        let mut iv: [u8; IV_LEN] = [0u8; IV_LEN];
        try!(reader.read(&mut iv).map_err(|io_err| PasswordError::Io(io_err)).and_then(|num_bytes| {
            if num_bytes == IV_LEN {
                Ok(())
            } else {
                Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")))
            }
        }));

        // Read the HMAC signature.
        let mut signature: [u8; SIGNATURE_LEN] = [0u8; SIGNATURE_LEN];
        try!(reader.read(&mut signature).map_err(|io_err| PasswordError::Io(io_err)).and_then(|num_bytes| {
            if num_bytes == SIGNATURE_LEN {
                Ok(())
            } else {
                Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unexpected eof")))
            }
        }));

        // The encrypted password data.
        let mut blob: Vec<u8> = Vec::new();
        try!(reader.read_to_end(&mut blob).map_err(|io_err| PasswordError::Io(io_err)));

        // Derive a 256 bits encryption key from the password.
        let scrypt_params = scrypt::ScryptParams::new(
            scrypt_log2_n,
            scrypt_r,
            scrypt_p
        );
        let key = generate_encryption_key(scrypt_params, master_password.deref(), salt);

        // Check the signature against what it should be.
        let new_signature_mac = try!(digest(key.deref(), version, scrypt_log2_n, scrypt_r, scrypt_p, &iv, &salt, blob.deref()).map_err(|io_err| {
            PasswordError::Io(io_err)
        })).result();
        let old_signature_mac = MacResult::new(&signature);
        if new_signature_mac != old_signature_mac {
            return Err(PasswordError::CorruptionError);
        }

        // Decrypt the data.
        let passwords = match aes::decrypt(blob.deref(), key.as_ref(), iv.as_ref()) {
            Ok(decrypted) => {
                let encoded = SafeString::new(String::from_utf8_lossy(decrypted.as_ref()).into_owned());
                match json::decode::<Schema>(encoded.deref()) {
                    Ok(json) => json.passwords,
                    Err(_) => {
                        return Err(PasswordError::InvalidJsonError);
                    }
                }
            },
            Err(_) => {
                return Err(PasswordError::DecryptionError);
            }
        };

        Ok(PasswordStore {
            key: key,
            scrypt_log2_n: scrypt_log2_n,
            scrypt_r: scrypt_r,
            scrypt_p: scrypt_p,
            salt: salt,
            schema: Schema {
                passwords: passwords,
            },
        })
    }

    pub fn sync(&self, file: &mut File) -> Result<(), PasswordError> {
        // This should never fail. The structs are all encodable.
        let json_schema = match json::encode(&self.schema) {
            Ok(json_schema) => json_schema,
            Err(_) => {
                return Err(PasswordError::InvalidJsonError);
            }
        };
        let json_schema = SafeString::new(json_schema);

        // Encrypt the data with a new salt and a new IV.
        let iv = try!(generate_random_iv().map_err(|io_err| PasswordError::Io(io_err)));
        let encrypted = match aes::encrypt(json_schema.deref().as_bytes(), self.key.as_ref(), iv.as_ref()) {
            Ok(val) => { val },
            Err(_) => { return Err(PasswordError::EncryptionError) }
        };

        // Reset the file pointer.
        try!(file.seek(SeekFrom::Start(0)).and_then(|_| file.set_len(0)).map_err(|err| PasswordError::Io(err)));

        // Write the file version.
        try!(match file.write_u32::<BigEndian>(VERSION) {
            Ok(_) => Ok(()),
            Err(err) => {
                match err {
                    ByteorderError::Io(err) => Err(PasswordError::Io(err)),
                    _ => Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unknown")))
                }
            }
        });

        // Write the scrypt params.
        try!(match file.write_u8(self.scrypt_log2_n) {
            Ok(_) => Ok(()),
            Err(err) => {
                match err {
                    ByteorderError::Io(err) => Err(PasswordError::Io(err)),
                    _ => Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unknown")))
                }
            }
        });
        try!(match file.write_u32::<BigEndian>(self.scrypt_r) {
            Ok(_) => Ok(()),
            Err(err) => {
                match err {
                    ByteorderError::Io(err) => Err(PasswordError::Io(err)),
                    _ => Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unknown")))
                }
            }
        });
        try!(match file.write_u32::<BigEndian>(self.scrypt_p) {
            Ok(_) => Ok(()),
            Err(err) => {
                match err {
                    ByteorderError::Io(err) => Err(PasswordError::Io(err)),
                    _ => Err(PasswordError::Io(IoError::new(IoErrorKind::Other, "unknown")))
                }
            }
        });


        // Write the key derivation salt.
        try!(file.write_all(&self.salt).map_err(|err| PasswordError::Io(err)));

        // Write the encryption IV.
        try!(file.write_all(&iv).map_err(|err| PasswordError::Io(err)));

        // Write the file signature.
        let signature = try!(digest(self.key.deref(), VERSION, self.scrypt_log2_n, self.scrypt_r, self.scrypt_p, &iv, &self.salt, encrypted.as_ref()).map_err(|io_err| {
            PasswordError::Io(io_err)
        })).result();
        try!(file.write_all(signature.code()).map_err(|err| PasswordError::Io(err)));

        // Write the encrypted password data.
        try!(file.write_all(&encrypted.as_ref()).map_err(|err| PasswordError::Io(err)));

        try!(file.sync_all().map_err(|err| PasswordError::Io(err)));
        Ok(())
    }

    pub fn get_all_passwords(&self) -> &[Password] {
        self.schema.passwords.deref()
    }

    /// Adds a password to the file.
    pub fn add_password(&mut self, password: Password)-> Result<(), PasswordError> {
        if self.has_password(password.name.deref()) {
            return Err(PasswordError::AppExistsError);
        }
        self.schema.passwords.push(password);
        Ok(())
    }

    pub fn delete_password(&mut self, name: &str) -> Result<Password, PasswordError> {
        let p = try!(self.get_password(name).ok_or(PasswordError::NoSuchAppError));

        let mut i = 0;
        while i < self.schema.passwords.len() {
            if self.schema.passwords[i].name == p.name {
                return Ok(self.schema.passwords.remove(i));
            }
            i += 1;
        }
        unreachable!();
    }

    pub fn get_password(&self, name: &str) -> Option<Password> {
        'passwords_loop: for p in self.schema.passwords.iter() {
            // Since the app name must be the same, we need the same length.
            if p.name.len() != name.len() {
                continue 'passwords_loop;
            }

            // We're looking for the exact same app name, without regard to casing.
            let mut i: usize = 0;
            while i < p.name.len() {
                let c1 = p.name.chars().nth(i).map(|c| c.to_lowercase().nth(0));
                let c2 = name.chars().nth(i).map(|c| c.to_lowercase().nth(0));
                if c1 != c2 {
                    continue 'passwords_loop;
                }
                i += 1;
            }
            return Some(p.clone());
        }
        None
    }

    pub fn has_password(&self, name: &str) -> bool {
        self.get_password(name).is_some()
    }

    pub fn change_master_password(&mut self, master_password: &str) {
        let scrypt_params = scrypt::ScryptParams::new(
            self.scrypt_log2_n,
            self.scrypt_r,
            self.scrypt_p
        );
        self.key = generate_encryption_key(scrypt_params, master_password, self.salt);
    }
}
