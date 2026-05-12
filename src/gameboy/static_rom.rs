use defmt::{info, warn};
use embedded_hal::delay::DelayNs;

use crate::hal::timer::Instant;
use crate::hal::timer::TimerDevice;
use core::cell::RefCell;

pub struct StaticRomManager<
    D: embedded_sdmmc::BlockDevice,
    T: embedded_sdmmc::TimeSource,
    DT: TimerDevice,
    DR: Fn(&mut D),
    const MAX_DIRS: usize,
    const MAX_FILES: usize,
    const MAX_VOLUMES: usize,
> {
    volume_manager: RefCell<embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>>,
    rom: &'static [u8],
    start_time: Instant,
    timer: crate::hal::Timer<DT>,
    device_reset: DR,
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > StaticRomManager<D, T, DT, DR, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    pub fn new(
        rom: &'static [u8],
        volume_manager: embedded_sdmmc::VolumeManager<D, T, MAX_DIRS, MAX_FILES, MAX_VOLUMES>,
        timer: crate::hal::Timer<DT>,
        device_reset: DR,
    ) -> Self {
        let result: StaticRomManager<D, T, DT, DR, MAX_DIRS, MAX_FILES, MAX_VOLUMES> = Self {
            rom,
            volume_manager: RefCell::new(volume_manager),
            start_time: timer.get_counter(),
            timer,
            device_reset,
        };

        result
    }
    fn save_internal(
        &mut self,
        game_title: &str,
        bank_index: u8,
        bank: &[u8],
    ) -> Result<(), embedded_sdmmc::Error<D::Error>> {
        info!("Saving ram bank: {}", bank_index);
        self.timer.delay_ms(10);

        let volume_manager = self.volume_manager.borrow_mut();
        volume_manager.device(|d| {
            (self.device_reset)(d);
        });

        let volume = volume_manager.open_volume(embedded_sdmmc::VolumeIdx(0))?;
        let root_directory = volume.open_root_dir()?;

        if root_directory.find_directory_entry("saves").is_err() {
            root_directory.make_dir_in_dir("saves")?;
        }
        let mut game_dir_name = game_title.replace(" ", "").to_lowercase();
        game_dir_name.truncate(game_dir_name.len().min(8));

        let save_directory = root_directory.open_dir("saves")?;
        if save_directory
            .find_directory_entry(game_dir_name.as_str())
            .is_err()
        {
            save_directory.make_dir_in_dir(game_dir_name.as_str())?;
        }
        let game_directory = save_directory.open_dir(game_dir_name.as_str())?;

        let bank_file = game_directory.open_file_in_dir(
            alloc::format!("{}", bank_index).as_str(),
            embedded_sdmmc::Mode::ReadWriteCreateOrTruncate,
        )?;

        bank_file.write(bank)?;
        Ok(())
    }
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > gb_core::hardware::rom::RomManager
    for StaticRomManager<D, T, DT, DR, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    #[inline(always)]
    fn read_from_offset(&self, seek_offset: usize, index: usize, _bank_number: u8) -> u8 {
        let address = seek_offset + index;
        self.rom[address]
    }

    fn clock(&self) -> u64 {
        let current_time = self.timer.get_counter();
        let diff = current_time - self.start_time;
        diff.to_micros()
    }

    fn save(&mut self, game_title: &str, bank_index: u8, bank: &[u8]) {
        let mut result = None;
        for _i in 0..5 {
            let inner_result = self.save_internal(game_title, bank_index, bank);
            if inner_result.is_ok() {
                result = Some(inner_result.unwrap());
                break;
            }
            let error = inner_result.err().take().unwrap();
            warn!(
                "Failed to save ram, retrying, {}",
                defmt::Debug2Format(&error)
            );
            self.timer.delay_ms(200);
        }
        if result.is_none() {
            defmt::error!("Failed to save ram state!");
        }
    }

    fn load_to_bank(&mut self, game_title: &str, bank_index: u8, bank: &mut [u8]) {
        info!("Loading ram bank: {}", bank_index);
        self.timer.delay_ms(10);
        let volume_manager = self.volume_manager.borrow_mut();
        volume_manager.device(|d| {
            (self.device_reset)(d);
        });
        

        let volume = volume_manager
            .open_volume(embedded_sdmmc::VolumeIdx(0))
            .unwrap();
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
    }
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > core::ops::Index<usize> for StaticRomManager<D, T, DT, DR, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    type Output = u8;

    fn index(&self, index: usize) -> &Self::Output {
        &self.rom[index as usize]
    }
}
impl<
        D: embedded_sdmmc::BlockDevice,
        T: embedded_sdmmc::TimeSource,
        DT: TimerDevice,
        DR: Fn(&mut D),
        const MAX_DIRS: usize,
        const MAX_FILES: usize,
        const MAX_VOLUMES: usize,
    > core::ops::Index<core::ops::Range<usize>>
    for StaticRomManager<D, T, DT, DR, MAX_DIRS, MAX_FILES, MAX_VOLUMES>
{
    type Output = [u8];

    fn index(&self, index: core::ops::Range<usize>) -> &Self::Output {
        return &self.rom[index];
    }
}
