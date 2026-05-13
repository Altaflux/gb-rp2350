use crate::rp_hal::hal;

use display_interface::{DataFormat, DisplayError, WriteOnlyDataCommand};

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::OutputPin;
use hal::dma::HalfWord;
use hal::pio::{PIOExt, PIO};
use hal::pio::{Running, StateMachine, StateMachineIndex, Tx};
use hal::pio::{Rx, UninitStateMachine};
type Result = core::result::Result<(), DisplayError>;
use hal::dma::Byte;
use hal::dma::SingleChannel;
use hal::pio::Stopped;
use rp235x_hal::timer::TimerDevice;

use super::DmaStreamer;

pub struct SpiPioDmaInterface<
    RS,
    P: PIOExt,
    SM1: StateMachineIndex,
    SM2: StateMachineIndex,
    CH1,
    CH2,
    TD: TimerDevice,
> {
    streamer: DmaStreamer<CH1, CH2>,
    mode: Option<PioMode<P, SM1, SM2>>,
    rs: RS,
    timer: crate::hal::Timer<TD>,
}

enum PioMode<P: PIOExt, SM1: StateMachineIndex, SM2: StateMachineIndex> {
    ByteMode(
        (
            PioContainer<P, SM1, Byte, Running>,
            PioContainer<P, SM2, HalfWord, Stopped>,
        ),
    ),
    HalfWordMode(
        (
            PioContainer<P, SM1, Byte, Stopped>,
            PioContainer<P, SM2, HalfWord, Running>,
        ),
    ),
}

struct PioContainer<P: PIOExt, SM: StateMachineIndex, TxSize, State> {
    sm: StateMachine<(P, SM), State>,
    tx: Tx<(P, SM), TxSize>,
    rx: Rx<(P, SM)>,
}
impl<RS, P, SM1, SM2, CH1, CH2, TD: TimerDevice> SpiPioDmaInterface<RS, P, SM1, SM2, CH1, CH2, TD>
where
    P: PIOExt,
    SM1: StateMachineIndex,
    SM2: StateMachineIndex,
    RS: OutputPin,
    CH1: SingleChannel,
    CH2: SingleChannel,
{
    pub fn new(
        clock_divider: (u16, u8),
        rs: RS,
        pio: &mut PIO<P>,
        sm1: UninitStateMachine<(P, SM1)>,
        sm2: UninitStateMachine<(P, SM2)>,
        clk: u8,
        tx: u8,
        streamer: DmaStreamer<CH1, CH2>,
        timer: crate::hal::Timer<TD>,
    ) -> Self {
        let video_program = pio::pio_asm!(".side_set 1 ", "out pins, 1 side 0 ", "nop side 1",);

        let video_program_installed = pio.install(&video_program.program).unwrap();
        let (mut sm_8b, rx_8b, tx_8b) =
            hal::pio::PIOBuilder::from_installed_program(video_program_installed)
                .out_pins(tx, 1)
                .side_set_pin_base(clk)
                .autopull(true)
                .pull_threshold(8)
                .out_shift_direction(hal::pio::ShiftDirection::Left)
                .in_shift_direction(hal::pio::ShiftDirection::Left)
                .buffers(hal::pio::Buffers::OnlyTx)
                .clock_divisor_fixed_point(clock_divider.0, clock_divider.1)
                .build(sm1);
        sm_8b.set_pindirs([(tx, hal::pio::PinDir::Output)]);
        sm_8b.set_pindirs([(clk, hal::pio::PinDir::Output)]);
        let video_program_installed = pio.install(&video_program.program).unwrap();
        let (mut sm_16b, rx_16b, tx_16b) =
            hal::pio::PIOBuilder::from_installed_program(video_program_installed)
                .out_pins(tx, 1)
                .side_set_pin_base(clk)
                .autopull(true)
                .pull_threshold(16)
                .out_shift_direction(hal::pio::ShiftDirection::Left)
                .in_shift_direction(hal::pio::ShiftDirection::Left)
                .buffers(hal::pio::Buffers::OnlyTx)
                .clock_divisor_fixed_point(clock_divider.0, clock_divider.1)
                .build(sm2);

        sm_16b.set_pindirs([(tx, hal::pio::PinDir::Output)]);
        sm_16b.set_pindirs([(clk, hal::pio::PinDir::Output)]);

        let byte_sm = PioContainer {
            sm: sm_8b.start(),
            tx: tx_8b.transfer_size(Byte),
            rx: rx_8b,
        };

        let half_word_sm = PioContainer {
            sm: sm_16b,
            tx: tx_16b.transfer_size(HalfWord),
            rx: rx_16b,
        };
        Self {
            streamer: streamer,
            rs,
            mode: Some(PioMode::ByteMode((byte_sm, half_word_sm))),
            timer,
        }
    }

    //   #[cold]
    fn set_8bit_mode(
        pio_mode: PioMode<P, SM1, SM2>,
    ) -> (
        PioContainer<P, SM1, Byte, Running>,
        PioContainer<P, SM2, HalfWord, Stopped>,
    ) {
        let new_mode = match pio_mode {
            PioMode::ByteMode(mode) => (mode.0, mode.1),
            PioMode::HalfWordMode(mode) => {
                let half_word_sm = PioContainer {
                    sm: mode.1.sm.stop(),
                    tx: mode.1.tx,
                    rx: mode.1.rx,
                };
                let byte_sm = PioContainer {
                    sm: mode.0.sm.start(),
                    tx: mode.0.tx,
                    rx: mode.0.rx,
                };
                (byte_sm, half_word_sm)
            }
        };
        new_mode
    }

    //  #[cold]
    fn set_16bit_mode(
        pio_mode: PioMode<P, SM1, SM2>,
    ) -> (
        PioContainer<P, SM1, Byte, Stopped>,
        PioContainer<P, SM2, HalfWord, Running>,
    ) {
        let new_mode = match pio_mode {
            PioMode::ByteMode(mode) => {
                let byte_sm = PioContainer {
                    sm: mode.0.sm.stop(),
                    tx: mode.0.tx,
                    rx: mode.0.rx,
                };
                let half_word_sm = PioContainer {
                    sm: mode.1.sm.start(),
                    tx: mode.1.tx,
                    rx: mode.1.rx,
                };
                (byte_sm, half_word_sm)
            }
            PioMode::HalfWordMode(mode) => (mode.0, mode.1),
        };
        new_mode
    }

    #[inline(always)]
    pub fn is_idle(&mut self) -> bool {
        let mode = self.mode.take().unwrap();
        let is_idle = match mode {
            PioMode::ByteMode(ref pio) => pio.0.tx.has_stalled(),
            PioMode::HalfWordMode(ref pio) => pio.1.tx.has_stalled(),
        };
        self.mode = Some(mode);
        is_idle
    }

    #[inline(always)]
    fn send_data(&mut self, words: DataFormat<'_>) -> Result
    where
        P: PIOExt,
        SM1: StateMachineIndex,
        SM2: StateMachineIndex,
        RS: OutputPin,
    {
        match words {
            DataFormat::U8(slice) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (mut byte_sm, half_byte_sm) = Self::set_8bit_mode(pio_mode);
                byte_sm.tx = self
                    .streamer
                    .stream_8b(byte_sm.tx, &mut slice.iter().cloned());
                self.mode = Some(PioMode::ByteMode((byte_sm, half_byte_sm)));
                Ok(())
            }
            DataFormat::U16(slice) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (byte_sm, mut half_byte_sm) = Self::set_16bit_mode(pio_mode);
                half_byte_sm.tx =
                    self.streamer
                        .stream_16b(half_byte_sm.tx, &mut slice.iter().cloned(), |d| d);
                self.mode = Some(PioMode::HalfWordMode((byte_sm, half_byte_sm)));
                Ok(())
            }
            DataFormat::U16LE(slice) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (byte_sm, mut half_byte_sm) = Self::set_16bit_mode(pio_mode);
                half_byte_sm.tx =
                    self.streamer
                        .stream_16b(half_byte_sm.tx, &mut (slice.iter().cloned()), |d| d);
                self.mode = Some(PioMode::HalfWordMode((byte_sm, half_byte_sm)));
                Ok(())
            }
            DataFormat::U16BE(slice) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (byte_sm, mut half_byte_sm) = Self::set_16bit_mode(pio_mode);
                half_byte_sm.tx =
                    self.streamer
                        .stream_16b(half_byte_sm.tx, &mut (slice.iter().cloned()), |d| d);
                self.mode = Some(PioMode::HalfWordMode((byte_sm, half_byte_sm)));
                Ok(())
            }
            DataFormat::U8Iter(iter) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (mut byte_sm, half_byte_sm) = Self::set_8bit_mode(pio_mode);
                byte_sm.tx = self.streamer.stream_8b(byte_sm.tx, iter);
                self.mode = Some(PioMode::ByteMode((byte_sm, half_byte_sm)));

                Ok(())
            }
            DataFormat::U16LEIter(iter) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (byte_sm, mut half_byte_sm) = Self::set_16bit_mode(pio_mode);
                half_byte_sm.tx = self.streamer.stream_16b(half_byte_sm.tx, iter, |d| d);
                self.mode = Some(PioMode::HalfWordMode((byte_sm, half_byte_sm)));

                Ok(())
            }
            DataFormat::U16BEIter(iter) => {
                let pio_mode = core::mem::replace(&mut self.mode, None).unwrap();
                let (byte_sm, mut half_byte_sm) = Self::set_16bit_mode(pio_mode);
                half_byte_sm.tx = self.streamer.stream_16b(half_byte_sm.tx, iter, |d| d);
                self.mode = Some(PioMode::HalfWordMode((byte_sm, half_byte_sm)));

                Ok(())
            }
            _ => Err(DisplayError::DataFormatNotImplemented),
        }
    }
}

impl<RS, P, SM1, SM2, CH1: SingleChannel, CH2: SingleChannel, TD: TimerDevice>
    mipidsi::interface::Interface for SpiPioDmaInterface<RS, P, SM1, SM2, CH1, CH2, TD>
where
    P: PIOExt,
    SM1: StateMachineIndex,
    SM2: StateMachineIndex,
    RS: OutputPin,
{
    type Word = u8;
    type Error = DisplayError;

    const KIND: mipidsi::interface::InterfaceKind = mipidsi::interface::InterfaceKind::Serial4Line;

    fn send_command(&mut self, command: u8, args: &[u8]) -> Result {
        while !self.is_idle() {
            crate::hal::arch::nop();
        }
        self.rs.set_low().map_err(|_| DisplayError::RSError)?;
        self.timer.delay_ns(9000);
        self.send_data(DataFormat::U8(
            core::slice::from_ref(&command)
        ))?;
        self.rs.set_high().map_err(|_| DisplayError::RSError)?;
        self.timer.delay_ns(9000);
        self.send_data(DataFormat::U8(args))?;
        Ok(())
    }

    fn send_pixels<const N: usize>(
        &mut self,
        pixels: impl IntoIterator<Item = [Self::Word; N]>,
    ) -> Result {
        while !self.is_idle() {
            crate::hal::arch::nop();
        }
        self.rs.set_high().map_err(|_| DisplayError::RSError)?;
        self.timer.delay_ns(9000);

        self.send_data(DataFormat::U8Iter(&mut pixels.into_iter().flatten()))?;
        Ok(())
    }

    fn send_repeated_pixel<const N: usize>(
        &mut self,
        pixel: [Self::Word; N],
        count: u32,
    ) -> Result {
        self.send_pixels((0..count).map(|_| pixel))
    }
}

impl<RS, P, SM1, SM2, CH1: SingleChannel, CH2: SingleChannel, TD: TimerDevice> WriteOnlyDataCommand
    for SpiPioDmaInterface<RS, P, SM1, SM2, CH1, CH2, TD>
where
    P: PIOExt,
    SM1: StateMachineIndex,
    SM2: StateMachineIndex,
    RS: OutputPin,
{
    #[inline(always)]
    fn send_commands(&mut self, cmd: display_interface::DataFormat<'_>) -> Result {
        while !self.is_idle() {
            crate::hal::arch::nop();
        }
        self.rs.set_low().map_err(|_| DisplayError::RSError)?;
        self.timer.delay_ns(9000);
        self.send_data(cmd)?;
        Ok(())
    }
    #[inline(always)]
    fn send_data(&mut self, buf: display_interface::DataFormat<'_>) -> Result {
        while !self.is_idle() {
            crate::hal::arch::nop();
        }
        self.rs.set_high().map_err(|_| DisplayError::RSError)?;
        self.timer.delay_ns(9000);
        self.send_data(buf)?;
        Ok(())
    }
}
