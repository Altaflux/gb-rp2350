use crate::hal::timer::Instant;
use core::cell::RefCell;

use crate::hal::timer::TimerDevice;
use alloc::{
    boxed::Box,
    string::{String, ToString},
};
use const_lru::ConstLru;
use defmt::{debug, info};
use embedded_hal::delay::DelayNs;
use embedded_sdmmc::{RawFile, RawVolume};

pub struct SdRomManager<
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
    DT: TimerDevice,
    DR: Fn(&mut D),
    const ROM_CACHE_SIZE: usize,
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> {
    _rom_name: String,
    volume_manager: RefCell<embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    raw_rom_file: RefCell<Option<RawFile>>,
    raw_volume: RefCell<Option<RawVolume>>,
    bank_0: Box<[u8; 0x4000]>,
    bank_lru: RefCell<ConstLru<u8, Box<[u8; 0x4000]>, ROM_CACHE_SIZE, u8>>,
    start_time: Instant,
    timer: RefCell<crate::hal::Timer<DT>>,
    device_reset: DR,
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const ROM_CACHE_SIZE: usize,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > SdRomManager<D, T, DT, DR, ROM_CACHE_SIZE, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    pub fn new(
        rom_name: &str,
        mut volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        timer: crate::hal::Timer<DT>,
        device_reset: DR,
    ) -> Self {
        let mut volume = volume_manager
            .open_volume(embedded_sdmmc::VolumeIdx(0))
            .unwrap();
        let mut root_dir = volume.open_root_dir().unwrap();
        let mut rom_file = root_dir
            .open_file_in_dir(rom_name, embedded_sdmmc::Mode::ReadOnly)
            .unwrap();

        let mut bank_0 = Box::new([0u8; 0x4000]);
        rom_file.seek_from_start(0u32).unwrap();
        rom_file.read(&mut *bank_0).unwrap();
        let raw_rom_file = rom_file.to_raw_file();
        root_dir.close().unwrap();
        let raw_volume = volume.to_raw_volume();

        let lru = ConstLru::new();

        let result: SdRomManager<D, T, DT, DR, ROM_CACHE_SIZE, MAX_DIRS, MAX_FILES, MAX_VOLUMES> =
            Self {
                _rom_name: rom_name.to_string(),
                bank_0: bank_0,
                volume_manager: RefCell::new(volume_manager),
                bank_lru: RefCell::new(lru),
                raw_volume: RefCell::new(Some(raw_volume)),
                raw_rom_file: RefCell::new(Some(raw_rom_file)),
                start_time: timer.get_counter(),
                timer: RefCell::new(timer),
                device_reset,
            };

        result
    }

    #[inline(always)]
    fn read_bank(
        &self,
        bank_offset: usize,
    ) -> Result<Box<[u8; 0x4000]>, embedded_sdmmc::Error<D::Error>> {
        let mut volume_manager = self.volume_manager.borrow_mut();

        let raw_file = self.raw_rom_file.take().unwrap();
        let mut file = raw_file.to_file(&mut volume_manager);

        let mut buffer: Box<[u8; 0x4000]> = Box::new([0u8; 0x4000]);

        file.seek_from_start(bank_offset as u32).unwrap();
        let read_result = file.read(&mut *buffer);

        self.raw_rom_file.replace(Some(file.to_raw_file()));
        read_result?;
        Ok(buffer)
    }
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const ROM_CACHE_SIZE: usize,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > gb_core::hardware::rom::RomManager
    for SdRomManager<D, T, DT, DR, ROM_CACHE_SIZE, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    #[inline(always)]
    fn read_from_offset(&self, seek_offset: usize, index: usize, bank_number: u8) -> u8 {
        if seek_offset == 0x0000 {
            return self.bank_0[index as usize];
        }
        let mut bank_lru = self.bank_lru.borrow_mut();
        let bank = bank_lru.get(&(bank_number));
        let value = match bank {
            Some(buffer) => buffer[index],
            None => {
                info!("Loading Rom Bank: {}", bank_number);
                let buffer: Box<[u8; 0x4000]> = self.read_bank(seek_offset).unwrap();
                let result = buffer[index];

                let unloaded_bank = bank_lru.insert(bank_number, buffer);
                if unloaded_bank.is_some() {
                    match unloaded_bank.unwrap() {
                        const_lru::InsertReplaced::LruEvicted(index, _) => {
                            debug!("Unloaded bank: {}", index);
                        }
                        const_lru::InsertReplaced::OldValue(_) => {
                            debug!("Unloaded bank: unknown");
                        }
                    }
                }
                result
            }
        };
        value
    }

    fn clock(&self) -> u64 {
        let current_time = self.timer.borrow().get_counter();
        let diff = current_time - self.start_time;
        diff.to_micros()
    }

    fn save(&mut self, game_title: &str, bank_index: u8, bank: &[u8]) {
        self.timer.borrow_mut().delay_ms(10);
        let mut volume_manager = self.volume_manager.borrow_mut();
        volume_manager.device(|d| {
            (self.device_reset)(d);
        });

        let volume = self
            .raw_volume
            .take()
            .unwrap()
            .to_volume(&mut volume_manager);
        let mut root_directory = volume.open_root_dir().unwrap();

        if root_directory.find_directory_entry("saves").is_err() {
            root_directory.make_dir_in_dir("saves").unwrap();
        }
        let mut game_dir_name = game_title.replace(" ", "").to_lowercase();
        game_dir_name.truncate(game_dir_name.len().min(8));

        let mut save_directory = root_directory.open_dir("saves").unwrap();
        if save_directory
            .find_directory_entry(game_dir_name.as_str())
            .is_err()
        {
            save_directory
                .make_dir_in_dir(game_dir_name.as_str())
                .unwrap();
        }
        let game_directory = save_directory.open_dir(game_dir_name.as_str()).unwrap();

        let bank_file = game_directory
            .open_file_in_dir(
                alloc::format!("{}", bank_index).as_str(),
                embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
            )
            .unwrap();

        bank_file.write(bank).unwrap();
        bank_file.close().unwrap();
        game_directory.close().unwrap();
        save_directory.close().unwrap();
        root_directory.close().unwrap();

        self.raw_volume.replace(Some(volume.to_raw_volume()));
    }

    fn load_to_bank(&mut self, game_title: &str, bank_index: u8, bank: &mut [u8]) {
        info!("Loading ram bank: {}", bank_index);
        self.timer.borrow_mut().delay_ms(10);
        let mut volume_manager = self.volume_manager.borrow_mut();
        volume_manager.device(|d| {
            (self.device_reset)(d);
        });

        let volume = self
            .raw_volume
            .take()
            .unwrap()
            .to_volume(&mut volume_manager);
        let root_directory = volume.open_root_dir().unwrap();

        if root_directory.find_directory_entry("saves").is_err() {
            root_directory.make_dir_in_dir("saves").unwrap();
        }
        let mut game_dir_name = game_title.replace(" ", "").to_lowercase();
        game_dir_name.truncate(game_dir_name.len().min(8));

        let mut save_directory = root_directory.open_dir("saves").unwrap();
        if save_directory
            .find_directory_entry(game_dir_name.as_str())
            .is_err()
        {
            save_directory
                .make_dir_in_dir(game_dir_name.as_str())
                .unwrap();
        }
        let game_directory = save_directory.open_dir(game_dir_name.as_str()).unwrap();

        let bank_name = alloc::format!("{}", bank_index);
        if game_directory
            .find_directory_entry(bank_name.as_str())
            .is_ok()
        {
            let bank_file = game_directory
                .open_file_in_dir(bank_name.as_str(), embedded_sdmmc::Mode::ReadOnly)
                .unwrap();

            bank_file.read(bank).unwrap();
            bank_file.close().unwrap();
        }

        game_directory.close().unwrap();
        save_directory.close().unwrap();
        root_directory.close().unwrap();

        self.raw_volume.replace(Some(volume.to_raw_volume()));
    }
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const ROM_CACHE_SIZE: usize,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > core::ops::Index<usize>
    for SdRomManager<D, T, DT, DR, ROM_CACHE_SIZE, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    type Output = u8;

    fn index(&self, index: usize) -> &Self::Output {
        &self.bank_0[index as usize]
    }
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const ROM_CACHE_SIZE: usize,
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > core::ops::Index<core::ops::Range<usize>>
    for SdRomManager<D, T, DT, DR, ROM_CACHE_SIZE, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    type Output = [u8];

    fn index(&self, index: core::ops::Range<usize>) -> &Self::Output {
        return &self.bank_0[index];
    }
}
