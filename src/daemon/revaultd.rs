use common::config::{config_folder_path, BitcoindConfig, Config, ConfigError};

use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt, fs,
    io::{self, Read, Write},
    net::SocketAddr,
    path::PathBuf,
    str::FromStr,
    time,
    vec::Vec,
};

use revault_net::{
    noise::{PublicKey as NoisePubKey, SecretKey as NoisePrivKey},
    sodiumoxide::{self, crypto::scalarmult::curve25519},
};
use revault_tx::{
    bitcoin::{
        secp256k1,
        util::bip32::{ChildNumber, ExtendedPubKey},
        Address, BlockHash, Script, TxOut,
    },
    miniscript::descriptor::{DescriptorPublicKey, DescriptorPublicKeyCtx},
    scripts::{
        cpfp_descriptor, deposit_descriptor, unvault_descriptor, CpfpDescriptor, DepositDescriptor,
        EmergencyAddress, UnvaultDescriptor,
    },
    transactions::{
        CancelTransaction, DepositTransaction, EmergencyTransaction, UnvaultEmergencyTransaction,
        UnvaultTransaction,
    },
};

/// The status of a [Vault], depends both on the block chain and the set of pre-signed
/// transactions
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VaultStatus {
    /// The deposit transaction has less than 6 confirmations
    Unconfirmed,
    // FIXME: Do we assume "no reorgs > 6 blocks" ?
    /// The deposit transaction has more than 6 confirmations
    Funded,
    /// The emergency transaction is signed
    Secured,
    /// The unvault transaction is signed (implies that the second emergency and the
    /// cancel transaction are signed).
    Active,
    /// The unvault transaction has been broadcast
    Unvaulting,
    /// The unvault transaction is confirmed
    Unvaulted,
    /// The cancel transaction has been broadcast
    Canceling,
    /// The cancel transaction is confirmed
    Canceled,
    /// The first emergency transactions has been broadcast
    EmergencyVaulting,
    /// The first emergency transactions is confirmed
    EmergencyVaulted,
    /// The unvault emergency transactions has been broadcast
    UnvaultEmergencyVaulting,
    /// The unvault emergency transactions is confirmed
    UnvaultEmergencyVaulted,
    /// The unvault transaction CSV is expired
    Spendable,
    /// The spend transaction has been broadcast
    Spending,
    // TODO: At what depth do we forget it ?
    /// The spend transaction is confirmed
    Spent,
}

impl TryFrom<u32> for VaultStatus {
    type Error = ();

    fn try_from(n: u32) -> Result<Self, Self::Error> {
        match n {
            0 => Ok(Self::Unconfirmed),
            1 => Ok(Self::Funded),
            2 => Ok(Self::Secured),
            3 => Ok(Self::Active),
            4 => Ok(Self::Unvaulting),
            5 => Ok(Self::Unvaulted),
            6 => Ok(Self::Canceling),
            7 => Ok(Self::Canceled),
            8 => Ok(Self::EmergencyVaulting),
            9 => Ok(Self::EmergencyVaulted),
            10 => Ok(Self::UnvaultEmergencyVaulting),
            11 => Ok(Self::UnvaultEmergencyVaulted),
            12 => Ok(Self::Spendable),
            13 => Ok(Self::Spending),
            14 => Ok(Self::Spent),
            _ => Err(()),
        }
    }
}

impl FromStr for VaultStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "unconfirmed" => Ok(Self::Unconfirmed),
            "funded" => Ok(Self::Funded),
            "secured" => Ok(Self::Secured),
            "active" => Ok(Self::Active),
            "unvaulting" => Ok(Self::Unvaulting),
            "unvaulted" => Ok(Self::Unvaulted),
            "canceling" => Ok(Self::Canceling),
            "canceled" => Ok(Self::Canceled),
            "emergencyvaulting" => Ok(Self::EmergencyVaulting),
            "emergencyvaulted" => Ok(Self::EmergencyVaulted),
            "unvaultermergencyvaulting" => Ok(Self::UnvaultEmergencyVaulting),
            "unvaultermergencyvaulted" => Ok(Self::UnvaultEmergencyVaulted),
            "spendable" => Ok(Self::Spendable),
            "spending" => Ok(Self::Spending),
            "spent" => Ok(Self::Spent),
            _ => Err(()),
        }
    }
}

impl fmt::Display for VaultStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match *self {
                Self::Unconfirmed => "unconfirmed",
                Self::Funded => "funded",
                Self::Secured => "secured",
                Self::Active => "active",
                Self::Unvaulting => "unvaulting",
                Self::Unvaulted => "unvaulted",
                Self::Canceling => "canceling",
                Self::Canceled => "canceled",
                Self::EmergencyVaulting => "emergencyvaulting",
                Self::EmergencyVaulted => "emergencyvaulted",
                Self::UnvaultEmergencyVaulting => "unvaultermergencyvaulting",
                Self::UnvaultEmergencyVaulted => "unvaultermergencyvaulted",
                Self::Spendable => "spendable",
                Self::Spending => "spending",
                Self::Spent => "spent",
            }
        )
    }
}

// An error related to the initialization of communication keys
#[derive(Debug)]
enum KeyError {
    ReadingKey(io::Error),
    WritingKey(io::Error),
    InvalidKeySize,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::ReadingKey(e) => write!(f, "Error reading Noise key: '{}'", e),
            Self::WritingKey(e) => write!(f, "Error writing Noise key: '{}'", e),
            Self::InvalidKeySize => write!(f, "Invalid Noise key length"),
        }
    }
}

impl std::error::Error for KeyError {}

// The communication keys are (for now) hot, so we just create it ourselves on first run.
fn read_or_create_noise_key(secret_file: PathBuf) -> Result<NoisePrivKey, KeyError> {
    let mut noise_secret = NoisePrivKey([0; 32]);

    if !secret_file.as_path().exists() {
        log::info!(
            "No Noise private key at '{:?}', generating a new one",
            secret_file
        );
        noise_secret = sodiumoxide::crypto::box_::gen_keypair().1;

        // We create it in read-only but open it in write only.
        let mut options = fs::OpenOptions::new();
        options = options.write(true).create_new(true).clone();
        // FIXME: handle Windows ACLs
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options = options.mode(0o400).clone();
        }

        let mut fd = options.open(secret_file).map_err(KeyError::WritingKey)?;
        fd.write_all(&noise_secret.as_ref())
            .map_err(KeyError::WritingKey)?;
    } else {
        let mut noise_secret_fd = fs::File::open(secret_file).map_err(KeyError::ReadingKey)?;
        noise_secret_fd
            .read_exact(&mut noise_secret.0)
            .map_err(KeyError::ReadingKey)?;
    }

    // TODO: have a decent memory management and mlock() the key

    assert!(noise_secret.0 != [0; 32]);
    Ok(noise_secret)
}

/// A vault is defined as a confirmed utxo paying to the Vault Descriptor for which
/// we have a set of pre-signed transaction (emergency, cancel, unvault).
/// Depending on its status we may not yet be in possession of part -or the entirety-
/// of the pre-signed transactions.
/// Likewise, depending on our role (manager or stakeholder), we may not have the
/// emergency transactions.
pub struct _Vault {
    pub deposit_txo: TxOut,
    pub status: VaultStatus,
    pub vault_tx: Option<DepositTransaction>,
    pub emergency_tx: Option<EmergencyTransaction>,
    pub unvault_tx: Option<UnvaultTransaction>,
    pub cancel_tx: Option<CancelTransaction>,
    pub unvault_emergency_tx: Option<UnvaultEmergencyTransaction>,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub struct BlockchainTip {
    pub height: u32,
    pub hash: BlockHash,
}

/// Our global state
pub struct RevaultD {
    // Bitcoind stuff
    /// Everything we need to know to talk to bitcoind
    pub bitcoind_config: BitcoindConfig,
    /// Last block we heard about
    pub tip: Option<BlockchainTip>,

    // Scripts stuff
    /// Who am i, and where am i in all this mess ?
    pub our_stk_xpub: Option<ExtendedPubKey>,
    pub our_man_xpub: Option<ExtendedPubKey>,
    /// The miniscript descriptor of vault's outputs scripts
    pub deposit_descriptor: DepositDescriptor<DescriptorPublicKey>,
    /// The miniscript descriptor of unvault's outputs scripts
    pub unvault_descriptor: UnvaultDescriptor<DescriptorPublicKey>,
    /// The miniscript descriptor of CPFP output scripts (in unvault and spend transaction)
    pub cpfp_descriptor: CpfpDescriptor<DescriptorPublicKey>,
    /// The Emergency address, only available if we are a stakeholder
    pub emergency_address: Option<EmergencyAddress>,
    /// We don't make an enormous deal of address reuse (we cancel to the same keys),
    /// however we at least try to generate new addresses once they're used.
    // FIXME: think more about desync reconciliation..
    pub current_unused_index: ChildNumber,
    /// The secp context required by the xpub one.. We'll eventually use it to verify keys.
    pub secp_ctx: secp256k1::Secp256k1<secp256k1::VerifyOnly>,
    /// The locktime to use on all created transaction. Always 0 for now.
    pub lock_time: u32,
    /// The CSV in the unvault_descriptor. Unfortunately segregated from the descriptor..
    pub unvault_csv: u32,
    // FIXME: this is a hack as we'll move out of specifying xpubs in the config. We should
    // have a way to get the managers / stakeholders keys out of a descriptor in revault_tx
    /// All the managers public keys
    pub managers_pubkeys: Vec<DescriptorPublicKey>,

    // Network stuff
    /// The static private key we use to establish connections to servers. We reuse it, but Trevor
    /// said it's fine! https://github.com/noiseprotocol/noise_spec/blob/master/noise.md#14-security-considerations
    pub noise_secret: NoisePrivKey,
    /// The ip:port the coordinator is listening on. TODO: Tor
    pub coordinator_host: SocketAddr,
    /// The static public key to enact the Noise channel with the Coordinator
    pub coordinator_noisekey: NoisePubKey,
    pub coordinator_poll_interval: time::Duration,
    /// The ip:port (TODO: Tor) and Noise public key of each cosigning server, only set if we are
    /// a manager.
    pub cosigs: Option<Vec<(SocketAddr, NoisePubKey)>>,

    // 'Wallet' stuff
    /// A map from a scriptPubKey to a derivation index. Used to retrieve the actual public
    /// keys used to generate a script from bitcoind until we can pass it xpub-expressed
    /// Miniscript descriptors.
    pub derivation_index_map: HashMap<Script, ChildNumber>,
    /// The id of the wallet used in the db
    pub wallet_id: Option<u32>,

    // Misc stuff
    /// We store all our data in one place, that's here.
    pub data_dir: PathBuf,
    /// Should we run as a daemon? (Default: yes)
    pub daemon: bool,
    // TODO: servers connection stuff
}

fn create_datadir(datadir_path: &PathBuf) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    return {
        use fs::DirBuilder;
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = DirBuilder::new();
        builder.mode(0o700).recursive(true).create(datadir_path)
    };

    #[cfg(not(unix))]
    return {
        // FIXME: make Windows secure (again?)
        fs::create_dir_all(datadir_path)
    };
}

impl RevaultD {
    /// Creates our global state by consuming the static configuration
    pub fn from_config(config: Config) -> Result<RevaultD, Box<dyn std::error::Error>> {
        let our_man_xpub = config.manager_config.as_ref().map(|x| x.xpub);
        let our_stk_xpub = config.stakeholder_config.as_ref().map(|x| x.xpub);
        // Config should have checked that!
        assert!(our_man_xpub.is_some() || our_stk_xpub.is_some());

        let managers_pubkeys = config.managers_xpubs;
        let stakeholders_pubkeys = config.stakeholders_xpubs;
        let cosigners_pubkeys = config.cosigners_keys;

        let deposit_descriptor = deposit_descriptor(stakeholders_pubkeys.clone())?;
        let unvault_descriptor = unvault_descriptor(
            stakeholders_pubkeys,
            managers_pubkeys.clone(),
            managers_pubkeys.len(),
            cosigners_pubkeys,
            config.unvault_csv,
        )?;
        let cpfp_descriptor = cpfp_descriptor(managers_pubkeys.clone())?;
        let emergency_address = config.stakeholder_config.map(|x| x.emergency_address);

        let mut data_dir = config.data_dir.unwrap_or(config_folder_path()?);
        data_dir.push(config.bitcoind_config.network.to_string());
        if !data_dir.as_path().exists() {
            if let Err(e) = create_datadir(&data_dir) {
                return Err(Box::from(ConfigError(format!(
                    "Could not create data dir '{:?}': {}.",
                    data_dir,
                    e.to_string()
                ))));
            }
        }
        data_dir = fs::canonicalize(data_dir)?;

        let data_dir_str = data_dir
            .to_str()
            .expect("Impossible: the datadir path is valid unicode");
        let noise_secret_file = [data_dir_str, "noise_secret"].iter().collect();
        let noise_secret = read_or_create_noise_key(noise_secret_file)?;

        // TODO: support hidden services
        let coordinator_host = SocketAddr::from_str(&config.coordinator_host)?;
        let coordinator_noisekey = config.coordinator_noise_key;
        let coordinator_poll_interval = config.coordinator_poll_seconds;

        let cosigs = config.manager_config.map(|config| {
            config
                .cosigners
                .into_iter()
                .map(|config| (config.host, config.noise_key))
                .collect()
        });

        let daemon = !matches!(config.daemon, Some(false));

        let secp_ctx = secp256k1::Secp256k1::verification_only();

        Ok(RevaultD {
            our_stk_xpub,
            our_man_xpub,
            deposit_descriptor,
            unvault_descriptor,
            managers_pubkeys,
            cpfp_descriptor,
            secp_ctx,
            data_dir,
            daemon,
            emergency_address,
            noise_secret,
            coordinator_host,
            coordinator_noisekey,
            coordinator_poll_interval,
            cosigs,
            lock_time: 0,
            unvault_csv: config.unvault_csv,
            bitcoind_config: config.bitcoind_config,
            tip: None,
            // Will be updated by the database
            current_unused_index: ChildNumber::from(0),
            // FIXME: we don't need SipHash for those, use a faster alternative
            derivation_index_map: HashMap::new(),
            // Will be updated soon (:tm:)
            wallet_id: None,
        })
    }

    fn file_from_datadir(&self, file_name: &str) -> PathBuf {
        let data_dir_str = self
            .data_dir
            .to_str()
            .expect("Impossible: the datadir path is valid unicode");

        [data_dir_str, file_name].iter().collect()
    }

    /// The context required for deriving keys. We don't use it, as it's redundant with the
    /// descriptor derivation, therefore the ChildNumber is always 0.
    pub fn xpub_ctx(&self) -> DescriptorPublicKeyCtx<'_, secp256k1::VerifyOnly> {
        DescriptorPublicKeyCtx::new(&self.secp_ctx, ChildNumber::from(0))
    }

    /// Our Noise static public key
    pub fn noise_pubkey(&self) -> NoisePubKey {
        let scalar = curve25519::Scalar(self.noise_secret.0);
        NoisePubKey(curve25519::scalarmult_base(&scalar).0)
    }

    pub fn vault_address(&self, child_number: ChildNumber) -> Address {
        self.deposit_descriptor
            .derive(child_number)
            .0
            .address(self.bitcoind_config.network, self.xpub_ctx())
            .expect("deposit_descriptor is a wsh")
    }

    pub fn unvault_address(&self, child_number: ChildNumber) -> Address {
        self.unvault_descriptor
            .derive(child_number)
            .0
            .address(self.bitcoind_config.network, self.xpub_ctx())
            .expect("unvault_descriptor is a wsh")
    }

    pub fn gap_limit(&self) -> u32 {
        100
    }

    pub fn watchonly_wallet_name(&self) -> Option<String> {
        self.wallet_id
            .map(|ref id| format!("revaultd-watchonly-wallet-{}", id))
    }

    pub fn log_file(&self) -> PathBuf {
        self.file_from_datadir("log")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.file_from_datadir("revaultd.pid")
    }

    pub fn db_file(&self) -> PathBuf {
        self.file_from_datadir("revaultd.sqlite3")
    }

    pub fn watchonly_wallet_file(&self) -> Option<String> {
        self.watchonly_wallet_name().map(|ref name| {
            self.file_from_datadir(name)
                .to_str()
                .expect("Valid utf-8")
                .to_string()
        })
    }

    pub fn rpc_socket_file(&self) -> PathBuf {
        self.file_from_datadir("revaultd_rpc")
    }

    pub fn is_stakeholder(&self) -> bool {
        self.our_stk_xpub.is_some()
    }

    pub fn is_manager(&self) -> bool {
        self.our_man_xpub.is_some()
    }

    pub fn deposit_address(&self) -> Address {
        self.vault_address(self.current_unused_index)
    }

    pub fn last_deposit_address(&self) -> Address {
        let raw_index: u32 = self.current_unused_index.into();
        // FIXME: this should fail instead of creating a hardened index
        self.vault_address(ChildNumber::from(raw_index + self.gap_limit()))
    }

    pub fn last_unvault_address(&self) -> Address {
        let raw_index: u32 = self.current_unused_index.into();
        // FIXME: this should fail instead of creating a hardened index
        self.unvault_address(ChildNumber::from(raw_index + self.gap_limit()))
    }

    /// All deposit addresses as strings up to the gap limit (100)
    pub fn all_deposit_addresses(&mut self) -> Vec<String> {
        self.derivation_index_map
            .keys()
            .map(|s| {
                Address::from_script(s, self.bitcoind_config.network)
                    .expect("Created from P2WSH address")
                    .to_string()
            })
            .collect()
    }

    /// All unvault addresses as strings up to the gap limit (100)
    pub fn all_unvault_addresses(&mut self) -> Vec<String> {
        let raw_index: u32 = self.current_unused_index.into();
        (0..raw_index + self.gap_limit())
            .map(|raw_index| {
                // FIXME: this should fail instead of creating a hardened index
                self.unvault_address(ChildNumber::from(raw_index))
                    .to_string()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::RevaultD;
    use common::config::Config;

    use std::path::PathBuf;

    #[test]
    fn test_from_config() {
        let mut path = PathBuf::from(file!()).parent().unwrap().to_path_buf();

        path.push("../../test_data/invalid_config.toml");
        Config::from_file(Some(path.clone())).expect_err("Parsing invalid config file");

        path.pop();
        path.push("valid_config.toml");
        let config = Config::from_file(Some(path)).expect("Parsing valid config file");
        RevaultD::from_config(config).expect("Creating state from config");
        // TODO: test actual fields..
    }
}
