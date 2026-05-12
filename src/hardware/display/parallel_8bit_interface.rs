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
use hal::timer::TimerDevice;

use super::DmaStreamer;
pub struct Parallel8BitDmaInterface<RS, P: PIOExt, SM: StateMachineIndex, CH1, CH2, TD: TimerDevice>
{
    sm: StateMachine<(P, SM), Running>,
    tx: Option<Tx<(P, SM), HalfWord>>,
    rx: Rx<(P, SM)>,
    labels: PIOLabelDefines,
    rs: RS,
    streamer: DmaStreamer<CH1, CH2>,
    timer: crate::hal::Timer<TD>,
}

impl<RS, P, SM, CH1, CH2, TD: TimerDevice> Parallel8BitDmaInterface<RS, P, SM, CH1, CH2, TD>
where
    P: PIOExt,
    SM: StateMachineIndex,
    RS: OutputPin,
    CH1: SingleChannel,
    CH2: SingleChannel,
{
    pub fn new(
        clock_divider: (u16, u8),
        rs: RS,
        pio: &mut PIO<P>,
        sm: UninitStateMachine<(P, SM)>,
        rw: u8,
        pins: (u8, u8),
        streamer: DmaStreamer<CH1, CH2>,
        timer: crate::hal::Timer<TD>,
    ) -> Self {
        let video_program = pio::pio_asm!(
            ".side_set 1 opt",
            "jmp start_8 side 1",
            ".wrap_target"

            "public start_tx:"
            "pull side 1",
            "out pins, 24 side 0  ",
            "nop side 1  ",
            "out pins, 8 side 0  ",
            "jmp start_tx side 1 ",

            "public start_8:"
            "pull side 1 ",
            "out pins, 32 side 0 ",
            "jmp start_8 side 1 ",
            ".wrap"
        );
        let out_pin_offset = ((1i16 - pins.0 as i16) + pins.1 as i16) as u8;
        let video_program_installed = pio.install(&video_program.program).unwrap();
        let program_offset = video_program_installed.offset();
        let (mut video_sm, rx, vid_tx) =
            hal::pio::PIOBuilder::from_installed_program(video_program_installed)
                .out_pins(pins.0, out_pin_offset)
                .side_set_pin_base(rw)
                .out_shift_direction(hal::pio::ShiftDirection::Left)
                .in_shift_direction(hal::pio::ShiftDirection::Left)
                .buffers(hal::pio::Buffers::OnlyTx)
                .clock_divisor_fixed_point(clock_divider.0, clock_divider.1)
                .build(sm);
        video_sm.set_pindirs((pins.0..pins.1 + 1 as u8).map(|n| (n, hal::pio::PinDir::Output)));
        video_sm.set_pindirs([(rw, hal::pio::PinDir::Output)]);

        let labels = PIOLabelDefines {
            program_offset: program_offset,
            bit_16: video_program.public_defines.start_tx,
            bit_8: video_program.public_defines.start_8,
        };

        Self {
            rs: rs,
            sm: video_sm.start(),
            rx: rx,
            tx: Some(vid_tx.transfer_size(HalfWord)),
            labels: labels,
            streamer,
            timer,
        }
    }

    fn set_8bit_mode(&mut self) {
        let instruction = pio::Instruction {
            operands: pio::InstructionOperands::JMP {
                condition: pio::JmpCondition::Always,
                address: self.labels.program_offset as u8 + self.labels.bit_8 as u8,
            },
            delay: 0,
            side_set: None,
        };
        self.sm.exec_instruction(instruction);
    }
    fn set_16bit_mode(&mut self) {
        let instruction = pio::Instruction {
            operands: pio::InstructionOperands::JMP {
                condition: pio::JmpCondition::Always,
                address: self.labels.program_offset as u8 + self.labels.bit_16 as u8,
            },
            delay: 0,
            side_set: None,
        };
        self.sm.exec_instruction(instruction);
    }

    #[inline(always)]
    pub fn is_idle(&mut self) -> bool {
        self.tx.as_ref().unwrap().has_stalled()
    }

    #[allow(dead_code)]
    pub fn free(self, pio: &mut PIO<P>) -> (UninitStateMachine<(P, SM)>, RS) {
        let (sm, prg) = self.sm.uninit(self.rx, self.tx.unwrap());
        pio.uninstall(prg);
        (sm, self.rs)
    }

    #[inline(always)]
    fn send_data(&mut self, words: DataFormat<'_>) -> Result {
        match words {
            DataFormat::U8(slice) => {
                self.set_8bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self
                    .streamer
                    .stream_8b(tx.transfer_size(Byte), &mut slice.iter().cloned());
                self.tx = Some(tx.transfer_size(HalfWord));
                Ok(())
            }
            DataFormat::U16(slice) => {
                self.set_16bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self
                    .streamer
                    .stream_16b(tx, &mut slice.iter().cloned(), |d| d);
                self.tx = Some(tx);
                Ok(())
            }
            DataFormat::U16BE(slice) => {
                self.set_16bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self
                    .streamer
                    .stream_16b(tx, &mut slice.iter().cloned(), |d| d);
                self.tx = Some(tx);
                Ok(())
            }
            DataFormat::U16LE(slice) => {
                self.set_16bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self
                    .streamer
                    .stream_16b(tx, &mut slice.iter().cloned(), |d| d);
                self.tx = Some(tx);
                Ok(())
            }
            DataFormat::U8Iter(iter) => {
                self.set_8bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self.streamer.stream_8b(tx.transfer_size(Byte), iter);
                self.tx = Some(tx.transfer_size(HalfWord));
                Ok(())
            }
            DataFormat::U16BEIter(iter) => {
                self.set_16bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self.streamer.stream_16b(tx, iter, |d| d);
                self.tx = Some(tx);
                Ok(())
            }
            DataFormat::U16LEIter(iter) => {
                self.set_16bit_mode();
                let tx = self.tx.take().unwrap();
                let tx = self.streamer.stream_16b(tx, iter, |d| d);
                self.tx = Some(tx);
                Ok(())
            }
            _ => Err(DisplayError::DataFormatNotImplemented),
        }
    }
}

impl<RS, P, SM, CH1, CH2, TD: TimerDevice> WriteOnlyDataCommand
    for Parallel8BitDmaInterface<RS, P, SM, CH1, CH2, TD>
where
    P: PIOExt,
    SM: StateMachineIndex,
    RS: OutputPin,
    CH1: SingleChannel,
    CH2: SingleChannel,
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
#[derive(Debug)]
struct PIOLabelDefines {
    pub program_offset: u8,
    pub bit_8: i32,
    pub bit_16: i32,
}
