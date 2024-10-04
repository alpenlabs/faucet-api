use std::{
    fs::{read, write},
    io,
    path::Path,
};

use rand::{thread_rng, Rng};
use tracing::info;

use crate::settings::SETTINGS;

pub type Seed = [u8; 32];
pub struct SavableSeed(Seed);

impl SavableSeed {
    fn save(&self) -> io::Result<()> {
        write(&SETTINGS.seed_file, self.0)?;
        info!("seed saved");
        Ok(())
    }

    fn read() -> io::Result<Option<Self>> {
        if Path::new(&SETTINGS.seed_file).exists() {
            let bytes = read(&SETTINGS.seed_file)?;
            Ok(bytes.try_into().map(Self).ok())
        } else {
            Ok(None)
        }
    }

    pub fn load_or_create() -> io::Result<Seed> {
        match Self::read() {
            Ok(Some(me)) => {
                info!("successfully loaded seed");
                Ok(me.0)
            }
            _ => {
                info!("couldn't load seed, generating new one");
                let me = Self(thread_rng().gen());
                me.save()?;
                Ok(me.0)
            }
        }
    }
}
