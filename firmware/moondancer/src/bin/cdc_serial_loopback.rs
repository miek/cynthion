#![no_std]
#![no_main]

use log::{debug, error, info, warn};

use smolusb::class::cdc;
use smolusb::device::{Speed, UsbDevice};
use smolusb::event::UsbEvent;
use smolusb::setup::SetupPacket;
use smolusb::traits::{
    ReadControl, ReadEndpoint, UnsafeUsbDriverOperations, UsbDriverOperations, WriteEndpoint,
    WriteRefEndpoint,
};

use moondancer::{hal, pac};
use pac::csr::interrupt;

// - constants ----------------------------------------------------------------

const MAX_CONTROL_RESPONSE_SIZE: usize = 8;

// - types --------------------------------------------------------------------

/// The UsbDataPacket struct represents a single packet of data
/// received from a USB port.
pub struct UsbDataPacket {
    pub interface: moondancer::UsbInterface,
    pub endpoint: u8,
    pub bytes_read: usize,
    pub buffer: [u8; moondancer::EP_MAX_PACKET_SIZE],
}

// - global static state ------------------------------------------------------

use heapless::mpmc::MpMcQueue as Queue;
use moondancer::event::InterruptEvent;

static EVENT_QUEUE: Queue<InterruptEvent, { moondancer::EP_MAX_ENDPOINTS }> = Queue::new();
static USB_RECEIVE_PACKET_QUEUE: Queue<UsbDataPacket, { moondancer::EP_MAX_ENDPOINTS }> =
    Queue::new();

#[inline(always)]
fn dispatch_event(event: InterruptEvent) {
    match EVENT_QUEUE.enqueue(event) {
        Ok(()) => (),
        Err(_) => {
            error!("MachineExternal - event queue overflow");
        }
    }
}

#[inline(always)]
fn dispatch_receive_packet(usb_receive_packet: UsbDataPacket) {
    match USB_RECEIVE_PACKET_QUEUE.enqueue(usb_receive_packet) {
        Ok(()) => (),
        Err(_) => {
            error!("MachineExternal - usb receive packet queue overflow");
        }
    }
}

// - MachineExternal interrupt handler ----------------------------------------

#[allow(non_snake_case)]
#[no_mangle]
fn MachineExternal() {
    use moondancer::UsbInterface::{Aux, Target};

    // peripherals
    let peripherals = unsafe { pac::Peripherals::steal() };
    let leds = &peripherals.LEDS;
    let usb0 = unsafe { hal::Usb0::summon() };
    let usb1 = unsafe { hal::Usb1::summon() };

    // debug
    let pending = interrupt::reg_pending();
    leds.output
        .write(|w| unsafe { w.output().bits(pending as u8) });

    // - Usb0 (Target) interrupts --
    if usb0.is_pending(pac::Interrupt::USB0) {
        usb0.clear_pending(pac::Interrupt::USB0);
        usb0.bus_reset();
    } else if usb0.is_pending(pac::Interrupt::USB0_EP_CONTROL) {
        let endpoint = usb0.ep_control.epno.read().bits() as u8;
        usb0.clear_pending(pac::Interrupt::USB0_EP_CONTROL);
        dispatch_event(InterruptEvent::Usb(
            Target,
            UsbEvent::ReceiveControl(endpoint),
        ));
    } else if usb0.is_pending(pac::Interrupt::USB0_EP_IN) {
        usb0.clear_pending(pac::Interrupt::USB0_EP_IN);
        // TODO something a little bit safer would be nice
        unsafe {
            usb0.clear_tx_ack_active();
        }
        dispatch_event(InterruptEvent::Interrupt(pac::Interrupt::USB0_EP_IN));
    } else if usb0.is_pending(pac::Interrupt::USB0_EP_OUT) {
        // read data from endpoint
        let endpoint = usb0.ep_out.data_ep.read().bits() as u8;
        let mut receive_packet = UsbDataPacket {
            interface: Target,
            endpoint,
            bytes_read: 0,
            buffer: [0_u8; moondancer::EP_MAX_PACKET_SIZE],
        };
        receive_packet.bytes_read = usb0.read(endpoint, &mut receive_packet.buffer);

        // clear pending IRQ after data is read
        usb0.clear_pending(pac::Interrupt::USB0_EP_OUT);

        // dispatch packet to main loop
        dispatch_receive_packet(receive_packet);

    // - Usb1 (Aux) interrupts --
    } else if usb1.is_pending(pac::Interrupt::USB1) {
        usb1.clear_pending(pac::Interrupt::USB1);
        usb1.bus_reset();
    } else if usb1.is_pending(pac::Interrupt::USB1_EP_CONTROL) {
        let endpoint = usb1.ep_control.epno.read().bits() as u8;
        usb1.clear_pending(pac::Interrupt::USB1_EP_CONTROL);
        dispatch_event(InterruptEvent::Usb(
            Aux,
            UsbEvent::ReceiveControl(endpoint),
        ));
    } else if usb1.is_pending(pac::Interrupt::USB1_EP_IN) {
        usb1.clear_pending(pac::Interrupt::USB1_EP_IN);
        // TODO something a little bit safer would be nice
        unsafe {
            usb1.clear_tx_ack_active();
        }
        dispatch_event(InterruptEvent::Interrupt(pac::Interrupt::USB1_EP_IN));
    } else if usb1.is_pending(pac::Interrupt::USB1_EP_OUT) {
        // read data from endpoint
        let endpoint = usb1.ep_out.data_ep.read().bits() as u8;
        let mut receive_packet = UsbDataPacket {
            interface: Aux,
            endpoint,
            bytes_read: 0,
            buffer: [0_u8; moondancer::EP_MAX_PACKET_SIZE],
        };
        receive_packet.bytes_read = usb1.read(endpoint, &mut receive_packet.buffer);

        // clear pending IRQ after data is read
        usb1.clear_pending(pac::Interrupt::USB1_EP_OUT);

        // dispatch packet to main loop
        dispatch_receive_packet(receive_packet);

    // - Unknown Interrupt --
    } else {
        dispatch_event(InterruptEvent::UnknownInterrupt(pending));
    }
}

// - main entry point ---------------------------------------------------------

#[cfg(feature = "vexriscv")]
#[riscv_rt::pre_init]
unsafe fn pre_main() {
    pac::cpu::vexriscv::flush_icache();
    #[cfg(feature = "vexriscv_dcache")]
    pac::cpu::vexriscv::flush_dcache();
}

#[riscv_rt::entry]
fn main() -> ! {
    let peripherals = pac::Peripherals::take().unwrap();
    let leds = &peripherals.LEDS;
    leds.output.write(|w| unsafe { w.output().bits(0x0) });

    // initialize logging
    let serial = hal::Serial::new(peripherals.UART);
    moondancer::log::init(serial);
    info!("logging initialized");

    // usb0: Target
    let mut usb0 = UsbDevice::<_, MAX_CONTROL_RESPONSE_SIZE>::new(
        hal::Usb0::new(
            peripherals.USB0,
            peripherals.USB0_EP_CONTROL,
            peripherals.USB0_EP_IN,
            peripherals.USB0_EP_OUT,
        ),
        cdc::DEVICE_DESCRIPTOR,
        cdc::CONFIGURATION_DESCRIPTOR_0,
        cdc::USB_STRING_DESCRIPTOR_0,
        cdc::USB_STRING_DESCRIPTORS,
    );
    usb0.set_device_qualifier_descriptor(cdc::DEVICE_QUALIFIER_DESCRIPTOR);
    usb0.set_other_speed_configuration_descriptor(cdc::OTHER_SPEED_CONFIGURATION_DESCRIPTOR_0);
    usb0.cb_vendor_request = Some(handle_vendor_request);
    usb0.cb_string_request = Some(handle_string_request);
    let speed = usb0.connect();
    info!("Connected USB0 device: {:?}", Speed::from(speed));

    // usb1: Aux
    let mut usb1 = UsbDevice::<_, MAX_CONTROL_RESPONSE_SIZE>::new(
        hal::Usb1::new(
            peripherals.USB1,
            peripherals.USB1_EP_CONTROL,
            peripherals.USB1_EP_IN,
            peripherals.USB1_EP_OUT,
        ),
        cdc::DEVICE_DESCRIPTOR,
        cdc::CONFIGURATION_DESCRIPTOR_0,
        cdc::USB_STRING_DESCRIPTOR_0,
        cdc::USB_STRING_DESCRIPTORS,
    );
    usb1.set_device_qualifier_descriptor(cdc::DEVICE_QUALIFIER_DESCRIPTOR);
    usb1.set_other_speed_configuration_descriptor(cdc::OTHER_SPEED_CONFIGURATION_DESCRIPTOR_0);
    usb1.cb_vendor_request = Some(handle_vendor_request);
    usb1.cb_string_request = Some(handle_string_request);
    let speed = usb1.connect();
    info!("Connected USB1 device: {:?}", Speed::from(speed));

    // enable interrupts
    unsafe {
        // set mstatus register: interrupt enable
        riscv::interrupt::enable();

        // set mie register: machine external interrupts enable
        riscv::register::mie::set_mext();

        // write csr: enable usb0 interrupts and events
        interrupt::enable(pac::Interrupt::USB0);
        interrupt::enable(pac::Interrupt::USB0_EP_CONTROL);
        interrupt::enable(pac::Interrupt::USB0_EP_IN);
        interrupt::enable(pac::Interrupt::USB0_EP_OUT);
        interrupt::enable(pac::Interrupt::USB1);
        interrupt::enable(pac::Interrupt::USB1_EP_CONTROL);
        interrupt::enable(pac::Interrupt::USB1_EP_IN);
        interrupt::enable(pac::Interrupt::USB1_EP_OUT);
        usb0.hal_driver.enable_interrupts();
        usb1.hal_driver.enable_interrupts();
    }

    // prime the usb OUT endpoints we'll be using
    usb0.hal_driver.ep_out_prime_receive(1);
    usb0.hal_driver.ep_out_prime_receive(2);
    usb1.hal_driver.ep_out_prime_receive(1);
    usb1.hal_driver.ep_out_prime_receive(2);

    info!("Peripherals initialized, entering main loop.");

    loop {
        if let Some(UsbDataPacket {
            interface,
            endpoint,
            bytes_read,
            buffer,
        }) = USB_RECEIVE_PACKET_QUEUE.dequeue()
        {
            use moondancer::UsbInterface::{Aux, Target};

            match (interface, endpoint, bytes_read, buffer) {
                // usb0 receive packet handler
                (Target, endpoint, bytes_read, buffer) => {
                    if endpoint != 0 {
                        debug!(
                            "Received {} bytes on usb0 endpoint: {} - {:?}",
                            bytes_read,
                            endpoint,
                            &buffer[0..8],
                        );
                        usb1.hal_driver
                            .write_ref(endpoint, buffer.iter().take(bytes_read).into_iter());
                        info!("Sent {} bytes to usb1 endpoint: {}", bytes_read, endpoint);
                    }
                    usb0.hal_driver.ep_out_prime_receive(endpoint);
                }

                // usb1 receive packet handler
                (Aux, endpoint, bytes_read, buffer) => {
                    if endpoint != 0 {
                        debug!(
                            "Received {} bytes on usb1 endpoint: {} - {:?}",
                            bytes_read,
                            endpoint,
                            &buffer[0..8],
                        );
                        usb0.hal_driver
                            .write_ref(endpoint, buffer.iter().take(bytes_read).into_iter());
                        info!("Sent {} bytes to usb0 endpoint: {}", bytes_read, endpoint);
                    }
                    usb1.hal_driver.ep_out_prime_receive(endpoint);
                }

                // unhandled
                _ => (),
            }
        }

        if let Some(event) = EVENT_QUEUE.dequeue() {
            use moondancer::event::InterruptEvent::Usb;
            use moondancer::UsbInterface::{Aux, Target};
            use smolusb::event::UsbEvent::*;

            match event {
                // Usb0 received a control event
                Usb(Target, event @ BusReset)
                | Usb(Target, event @ ReceiveControl(0))
                | Usb(Target, event @ ReceivePacket(0))
                | Usb(Target, event @ SendComplete(0)) => {
                    debug!("\n\nUsb(Target, {:?})", event);
                    match usb0.dispatch_control(event) {
                        Ok(Some(control_event)) => {
                            // handle any events control couldn't
                            warn!("Unhandled control event on Target: {:?}", control_event);
                        }
                        Ok(None) => {
                            // control event was handled by UsbDevice
                        }
                        Err(e) => {
                            error!("Error handling control event on Target: {:?}", e);
                        }
                    }
                }

                // Usb1 received a control event
                Usb(Aux, event @ BusReset)
                | Usb(Aux, event @ ReceiveControl(0))
                | Usb(Aux, event @ ReceivePacket(0))
                | Usb(Aux, event @ SendComplete(0)) => {
                    debug!("\n\nUsb(Aux, {:?})", event);
                    match usb1.dispatch_control(event) {
                        Ok(Some(control_event)) => {
                            // handle any events control couldn't
                            warn!("Unhandled control event on Aux: {:?}", control_event);
                        }
                        Ok(None) => {
                            // control event was handled by UsbDevice
                        }
                        Err(e) => {
                            error!("Error handling control event on Aux: {:?}", e);
                        }
                    }
                }

                // unhandled
                _ => (),
            }
        }
    }
}

// - vendor request handlers --------------------------------------------------

fn handle_vendor_request<'a, D>(
    device: &UsbDevice<'a, D, MAX_CONTROL_RESPONSE_SIZE>,
    _setup_packet: &SetupPacket,
    request: u8,
) where
    D: ReadControl + ReadEndpoint + WriteEndpoint + WriteRefEndpoint + UsbDriverOperations,
{
    let request = cdc::ch34x::VendorRequest::from(request);
    debug!("  CDC-SERIAL vendor_request: {:?}", request);

    // we can just spoof these
    device.hal_driver.write(0, [0, 0].into_iter());
}

fn handle_string_request<'a, D>(
    device: &UsbDevice<'a, D, MAX_CONTROL_RESPONSE_SIZE>,
    _setup_packet: &SetupPacket,
    index: u8,
) where
    D: ReadControl + ReadEndpoint + WriteEndpoint + WriteRefEndpoint + UsbDriverOperations,
{
    debug!("  CDC-SERIAL string_request: {}", index);

    // we can just spoof this too
    device.hal_driver.write(0, [].into_iter());
}
