use crate::rp_hal::hal;

use hal::dma::{
    double_buffer::{Config, Transfer},
    ReadTarget, SingleChannel, WriteTarget,
};

use crate::hal::dma::{double_buffer::ReadNext, EndlessWriteTarget};
use embedded_dma::{ReadBuffer, Word};

use super::LineTransfer;

enum DmaState<
    T: Word,
    CH1: SingleChannel,
    CH2: SingleChannel,
    FROM: ReadTarget<ReceivedWord = T>,
    TO: WriteTarget<TransmittedWord = T> + EndlessWriteTarget,
> {
    IDLE(Transfer<CH1, CH2, FROM, TO, ()>),
    RUNNING(Transfer<CH1, CH2, FROM, TO, ReadNext<FROM>>),
}

pub struct DmaTransfer<
    T: 'static + Word,
    CH1: SingleChannel,
    CH2: SingleChannel,
    TO: WriteTarget<TransmittedWord = T> + EndlessWriteTarget,
> {
    dma: Option<DmaState<T, CH1, CH2, LimitingArrayReadTarget<T>, TO>>,
    second_buffer: Option<LimitingArrayReadTarget<T>>,
}

impl<T, CH1, CH2, TO> DmaTransfer<T, CH1, CH2, TO>
where
    T: 'static + Word,
    CH1: SingleChannel,
    CH2: SingleChannel,
    TO: WriteTarget<TransmittedWord = T> + EndlessWriteTarget,
{
    pub fn new(
        dma_channel: CH1,
        dma_channel2: CH2,
        tx: TO,
        buffer: &'static mut [T],
        buffer2: &'static mut [T],
    ) -> Self {
        let mut cfg = Config::new(
            (dma_channel, dma_channel2),
            LimitingArrayReadTarget::new(buffer, 0),
            tx,
        );
        Self {
            dma: (Some(DmaState::IDLE(cfg.start()))),
            second_buffer: Some(LimitingArrayReadTarget::new(buffer2, 0)),
        }
    }

    #[inline(always)]
    pub fn do_tranfer(&mut self, buffer: &'static mut [T], max: u32) -> &'static mut [T] {
        let dma_state = core::mem::replace(&mut self.dma, None).unwrap();
        let new_data: LimitingArrayReadTarget<T> = LimitingArrayReadTarget::new(buffer, max);
        let (free_buffer, new_dma_state) = match dma_state {
            DmaState::IDLE(transfer) => {
                let state = transfer.read_next(new_data);
                let second_buffer = core::mem::replace(&mut self.second_buffer, None).unwrap();
                (second_buffer, state)
            }
            DmaState::RUNNING(transfer) => {
                let (free_buffer, dma) = transfer.wait();
                let state = dma.read_next(new_data);
                (free_buffer, state)
            }
        };
        self.dma = Some(DmaState::RUNNING(new_dma_state));
        free_buffer.free()
    }

    pub fn wait_for_completion(&mut self) {
        let dma_state = core::mem::replace(&mut self.dma, None).unwrap();

        let new_dma_state = match dma_state {
            DmaState::IDLE(transfer) => transfer,
            DmaState::RUNNING(transfer) => {
                let (free_buffer, dma) = transfer.wait();
                self.second_buffer = Some(free_buffer);
                dma
            }
        };
        self.dma = Some(DmaState::IDLE(new_dma_state))
    }

    pub fn free(mut self) -> (CH1, CH2, TO, &'static mut [T], &'static mut [T]) {
        let dma_state = core::mem::replace(&mut self.dma, None).unwrap();

        match dma_state {
            DmaState::IDLE(transfer) => {
                let rs: (CH1, CH2, LimitingArrayReadTarget<T>, TO) = transfer.wait();
                let second_buffer = core::mem::replace(&mut self.second_buffer, None).unwrap();
                (rs.0, rs.1, rs.3, rs.2.free(), second_buffer.free())
            }
            DmaState::RUNNING(transfer) => {
                let rs: (
                    LimitingArrayReadTarget<T>,
                    Transfer<CH1, CH2, LimitingArrayReadTarget<T>, TO, ()>,
                ) = transfer.wait();

                let dma2: (CH1, CH2, LimitingArrayReadTarget<T>, TO) = rs.1.wait();

                (dma2.0, dma2.1, dma2.3, rs.0.free(), dma2.2.free())
            }
        }
    }
}

impl<T, CH1, CH2, TO> LineTransfer for DmaTransfer<T, CH1, CH2, TO>
where
    T: Word,
    CH1: SingleChannel,
    CH2: SingleChannel,
    TO: WriteTarget<TransmittedWord = T> + EndlessWriteTarget,
{
    type Item = T;

    fn send_scanline(
        &mut self,
        line: &'static mut [Self::Item],
        max: u32,
    ) -> &'static mut [Self::Item] {
        self.do_tranfer(line, max)
    }

    fn wait(&mut self) {
        self.wait_for_completion();
    }
}

struct LimitingArrayReadTarget<T: Word + 'static> {
    array: &'static mut [T],
    max_read: u32,
}

impl<T: Word + 'static> LimitingArrayReadTarget<T> {
    fn new(array: &'static mut [T], max_read: u32) -> Self {
        Self { array, max_read }
    }

    fn free(self) -> &'static mut [T] {
        self.array
    }
}

unsafe impl<T: Word + 'static> ReadTarget for LimitingArrayReadTarget<T> {
    type ReceivedWord = T;

    fn rx_treq() -> Option<u8> {
        None
    }

    fn rx_address_count(&self) -> (u32, u32) {
        let (ptr, _) = unsafe { self.array.read_buffer() };
        (ptr as u32, self.max_read as u32)
    }

    fn rx_increment(&self) -> bool {
        self.array.rx_increment()
    }
}
