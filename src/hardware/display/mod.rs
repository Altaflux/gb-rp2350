mod dma_streamer;
mod dma_transfer;
mod parallel_8bit_interface;
mod scaler;
mod spi_pio_interface;

pub use dma_streamer::DmaStreamer;
use dma_transfer::DmaTransfer;
#[allow(unused_imports)]
pub use parallel_8bit_interface::Parallel8BitDmaInterface;
pub use scaler::ScreenScaler;
#[allow(unused_imports)]
pub use spi_pio_interface::SpiPioDmaInterface;

trait LineTransfer {
    type Item;
    fn send_scanline(
        &mut self,
        line: &'static mut [Self::Item],
        size: u32,
    ) -> &'static mut [Self::Item];

    fn wait(&mut self);
}
