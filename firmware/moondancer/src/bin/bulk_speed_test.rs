#![no_std]
#![no_main]

use heapless::mpmc::MpMcQueue as Queue;
use log::{debug, error, info, warn};

use libgreat::{GreatError, GreatResult};

use smolusb::descriptor::*;
use smolusb::device::UsbDevice;
use smolusb::event::UsbEvent;
use smolusb::traits::{ReadEndpoint, UnsafeUsbDriverOperations, UsbDriverOperations};

use moondancer::event::InterruptEvent;
use moondancer::{hal, pac};

// - constants ----------------------------------------------------------------

const MAX_CONTROL_RESPONSE_SIZE: usize = 8;

// - global static state ------------------------------------------------------

static EVENT_QUEUE: Queue<InterruptEvent, 32> = Queue::new();

#[inline(always)]
fn dispatch_event(event: InterruptEvent) {
    match EVENT_QUEUE.enqueue(event) {
        Ok(()) => (),
        Err(_) => {
            error!("MachineExternal - event queue overflow");
            panic!("MachineExternal - event queue overflow");
        }
    }
}

// - MachineExternal interrupt handler ----------------------------------------

#[allow(non_snake_case)]
#[no_mangle]
fn MachineExternal() {
    use moondancer::UsbInterface::Target;

    let usb0 = unsafe { hal::Usb0::summon() };

    // - usb0 interrupts - "host_phy" / "aux_phy" --

    // USB0 UsbBusReset
    if usb0.is_pending(pac::Interrupt::USB0) {
        usb0.clear_pending(pac::Interrupt::USB0);
        usb0.bus_reset();
        dispatch_event(InterruptEvent::Usb(Target, UsbEvent::BusReset))

    // USB0_EP_CONTROL UsbReceiveSetupPacket
    } else if usb0.is_pending(pac::Interrupt::USB0_EP_CONTROL) {
        let endpoint = usb0.ep_control.epno.read().bits() as u8;
        usb0.clear_pending(pac::Interrupt::USB0_EP_CONTROL);
        dispatch_event(InterruptEvent::Usb(
            Target,
            UsbEvent::ReceiveControl(endpoint),
        ));

    // USB0_EP_OUT UsbReceiveData
    } else if usb0.is_pending(pac::Interrupt::USB0_EP_OUT) {
        let endpoint = usb0.ep_out.data_ep.read().bits() as u8;

        // discard packets from Bulk OUT transfer endpoint
        /*if endpoint == 1 {
            /*while usb0.ep_out.have.read().have().bit() {
                let _b = usb0.ep_out.data.read().data().bits();
            }*/
            usb0.ep_out_prime_receive(1);
            usb0.clear_pending(pac::Interrupt::USB0_EP_OUT);
            return;
        }*/

        usb0.clear_pending(pac::Interrupt::USB0_EP_OUT);
        dispatch_event(InterruptEvent::Usb(
            Target,
            UsbEvent::ReceivePacket(endpoint),
        ));

    // USB0_EP_IN UsbTransferComplete
    } else if usb0.is_pending(pac::Interrupt::USB0_EP_IN) {
        let endpoint = usb0.ep_in.epno.read().bits() as u8;
        usb0.clear_pending(pac::Interrupt::USB0_EP_IN);

        // TODO something a little bit safer would be nice
        unsafe {
            usb0.clear_tx_ack_active();
        }

        dispatch_event(InterruptEvent::Usb(
            Target,
            UsbEvent::SendComplete(endpoint),
        ));

    // - Unknown Interrupt --
    } else {
        let pending = pac::csr::interrupt::reg_pending();
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
    match main_loop() {
        Ok(()) => {
            error!("Firmware exited unexpectedly in main loop");
            panic!("Firmware exited unexpectedly in main loop")
        }
        Err(e) => {
            error!("Fatal error in firmware main loop: {}", e);
            panic!("Fatal error in firmware main loop: {}", e)
        }
    }
}

// - main loop ----------------------------------------------------------------

fn main_loop() -> GreatResult<()> {
    let peripherals = pac::Peripherals::take().unwrap();
    let leds = &peripherals.LEDS;

    // initialize logging
    moondancer::log::init(hal::Serial::new(peripherals.UART));
    info!("Logging initialized");

    // usb0: Target
    let mut usb0 = UsbDevice::<_, MAX_CONTROL_RESPONSE_SIZE>::new(
        hal::Usb0::new(
            peripherals.USB0,
            peripherals.USB0_EP_CONTROL,
            peripherals.USB0_EP_IN,
            peripherals.USB0_EP_OUT,
        ),
        USB_DEVICE_DESCRIPTOR,
        USB_CONFIGURATION_DESCRIPTOR_0,
        USB_STRING_DESCRIPTOR_0,
        USB_STRING_DESCRIPTORS,
    );
    usb0.set_device_qualifier_descriptor(USB_DEVICE_QUALIFIER_DESCRIPTOR);
    usb0.set_other_speed_configuration_descriptor(USB_OTHER_SPEED_CONFIGURATION_DESCRIPTOR_0);
    let speed = usb0.connect();
    debug!("Connected usb0 device: {:?}", speed);

    // enable interrupts
    unsafe {
        // set mstatus register: interrupt enable
        riscv::interrupt::enable();

        // set mie register: machine external interrupts enable
        riscv::register::mie::set_mext();

        // write csr: enable usb0 interrupts and events
        pac::csr::interrupt::enable(pac::Interrupt::USB0);
        pac::csr::interrupt::enable(pac::Interrupt::USB0_EP_CONTROL);
        pac::csr::interrupt::enable(pac::Interrupt::USB0_EP_IN);
        pac::csr::interrupt::enable(pac::Interrupt::USB0_EP_OUT);
        usb0.hal_driver.enable_interrupts();
    }

    info!("Peripherals initialized, entering main loop.");

    let mut test_command = TestCommand::Stop;
    let mut test_stats = TestStats::new();

    // 4 MB/s
    let test_data = {
        let mut test_data = [0_u8; moondancer::EP_MAX_PACKET_SIZE];
        for n in 0..moondancer::EP_MAX_PACKET_SIZE {
            test_data[n] = (n % 256) as u8;
        }
        test_data
    };

    // prime the usb OUT endpoints we'll be using
    usb0.hal_driver.ep_out_prime_receive(1);
    usb0.hal_driver.ep_out_prime_receive(2);

    let mut counter = 0;

    let mut rx_buffer: [u8; moondancer::EP_MAX_PACKET_SIZE] = [0; moondancer::EP_MAX_PACKET_SIZE];

    loop {
        let mut queue_length = 0;

        while let Some(event) = EVENT_QUEUE.dequeue() {
            use moondancer::{event::InterruptEvent::*, UsbInterface::Target};
            use smolusb::event::UsbEvent::*;

            leds.output.write(|w| unsafe { w.output().bits(0) });

            match event {
                // - usb0 event handlers --

                // Usb0 received a control event
                Usb(Target, event @ BusReset)
                | Usb(Target, event @ ReceiveControl(0))
                | Usb(Target, event @ ReceivePacket(0))
                | Usb(Target, event @ SendComplete(0)) => {
                    debug!("\n\nUsb(Target, {:?})", event);
                    match usb0
                        .dispatch_control(event)
                        .map_err(|_| GreatError::IoError)?
                    {
                        Some(control_event) => {
                            // handle any events control couldn't
                            warn!("Unhandled control event: {:?}", control_event);
                        }
                        None => {
                            // control event was handled by UsbDevice
                        }
                    }
                }

                // Usb0 received packet
                Usb(Target, ReceivePacket(endpoint)) => {
                    let bytes_read = usb0.hal_driver.read(endpoint, &mut rx_buffer);
                    if endpoint == 1 {
                        leds.output.write(|w| unsafe { w.output().bits(0b11_1000) });
                        if counter % 100 == 0 {
                            log::trace!(
                                "{:?} .. {:?}",
                                &rx_buffer[0..8],
                                &rx_buffer[(bytes_read - 8)..]
                            );
                        }
                        counter += 1;
                        usb0.hal_driver.ep_out_prime_receive(1);
                    } else if endpoint == 2 {
                        info!("received command data from host: {} bytes", bytes_read);
                        let command = rx_buffer[0].into();
                        match (bytes_read, &command) {
                            (1, TestCommand::In) => {
                                info!("starting test: IN");
                                test_stats.reset();
                                test_command = TestCommand::In;
                            }
                            (1, TestCommand::Out) => {
                                info!("starting test: OUT");
                                test_stats.reset();
                                test_command = TestCommand::Out;
                            }
                            (1, command) => {
                                info!("stopping test: {:?}", command);
                                info!("  max write time: {}", test_stats.max_write_time);
                                info!("  min write time: {}", test_stats.min_write_time);
                                info!("  max flush time: {}", test_stats.max_flush_time);
                                info!("  min flush time: {}", test_stats.min_flush_time);
                                info!("  write count: {}", test_stats.write_count);
                                info!("  reset count: {}", test_stats.reset_count);
                                test_command = TestCommand::Stop;
                            }
                            (bytes_read, _) => {
                                error!(
                                    "received invalid command from host: {:?} (read {} bytes)",
                                    command, bytes_read,
                                );
                            }
                        }
                        usb0.hal_driver.ep_out_prime_receive(2);
                    } else {
                        usb0.hal_driver.ep_out_prime_receive(endpoint);
                    }
                }

                // Usb0 transfer complete
                Usb(Target, SendComplete(_endpoint)) => {
                    leds.output.write(|w| unsafe { w.output().bits(0b00_0111) });
                }

                // Error Message
                ErrorMessage(message) => {
                    error!("MachineExternal Error - {}", message);
                }

                // Unhandled event
                _ => {
                    error!("Unhandled event: {:?}", event);
                }
            }

            queue_length += 1;
        }

        // perform tests
        match test_command {
            TestCommand::In => test_in_speed(leds, &usb0.hal_driver, &test_data, &mut test_stats),
            TestCommand::Out => (),
            _ => (),
        }

        // queue diagnostics
        if queue_length > test_stats.max_queue_length {
            test_stats.max_queue_length = queue_length;
            debug!("max_queue_length: {}", test_stats.max_queue_length);
        }
    }
}

// - tests --------------------------------------------------------------------

/// Send test data to host as fast as possible
#[inline(always)]
fn test_in_speed(
    _leds: &pac::LEDS,
    usb0: &hal::Usb0,
    test_data: &[u8; moondancer::EP_MAX_PACKET_SIZE],
    test_stats: &mut TestStats,
) {
    // Passing in a fixed size slice ref is 4MB/s vs 3.7MB/s
    #[inline(always)]
    fn test_write_slice(
        usb0: &hal::Usb0,
        endpoint: u8,
        data: &[u8; moondancer::EP_MAX_PACKET_SIZE],
    ) -> bool {
        let mut did_reset = false;
        if usb0.ep_in.have.read().have().bit() {
            usb0.ep_in.reset.write(|w| w.reset().bit(true));
            did_reset = true;
        }
        // 5.033856452242371MB/s.
        for byte in data.iter() {
            usb0.ep_in.data.write(|w| unsafe { w.data().bits(*byte) });
        }
        // 6.392375785142406MB/s. - no memory access
        /*for n in 0..moondancer::EP_MAX_PACKET_SIZE {
            usb0.ep_in.data.write(|w| unsafe { w.data().bits((n % 256) as u8) });
        }*/
        usb0.ep_in
            .epno
            .write(|w| unsafe { w.epno().bits(endpoint & 0xf) });
        did_reset
    }

    // wait for fifo endpoint to be idle
    let (_, t_flush) = moondancer::profile!(
        let mut timeout = 100;
        while !usb0.ep_in.idle.read().idle().bit() && timeout > 0 {
            timeout -= 1;
        }
    );

    // write data to endpoint fifo
    let (did_reset, t_write) = moondancer::profile!(
        //usb0.write(0x1, test_data.into_iter().copied()); false // 6780 / 5653 ~3.99MB/s
        //usb0.write_ref(0x1, test_data.iter()); false // 5663 / 5652 - ~4.02MB/s
        test_write_slice(usb0, 0x1, test_data) // 56533 / 5652 - ~4.04MB/s
    );
    test_stats.write_count += 1;

    // update stats
    test_stats.update_in(t_write, t_flush, did_reset);
}

// - types --------------------------------------------------------------------

#[derive(Debug, PartialEq)]
#[repr(u8)]
enum TestCommand {
    Stop,
    In = 0x23,
    Out = 0x42,
    Error = 0xff,
}

impl From<u8> for TestCommand {
    fn from(value: u8) -> Self {
        match value {
            0x23 => TestCommand::In,
            0x42 => TestCommand::Out,
            0xff => TestCommand::Error,
            _ => TestCommand::Stop,
        }
    }
}

struct TestStats {
    max_queue_length: usize,

    max_write_time: usize,
    min_write_time: usize,
    max_flush_time: usize,
    min_flush_time: usize,

    write_count: usize,
    reset_count: usize,
}

impl TestStats {
    const fn new() -> Self {
        Self {
            max_queue_length: 0,
            max_write_time: 0,
            min_write_time: usize::MAX,
            max_flush_time: 0,
            min_flush_time: usize::MAX,
            write_count: 0,
            reset_count: 0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    #[inline(always)]
    fn update_in(&mut self, t_write: usize, t_flush: usize, did_reset: bool) {
        if t_write > self.max_write_time {
            self.max_write_time = t_write;
        }
        if t_write < self.min_write_time {
            self.min_write_time = t_write;
        }
        if t_flush > self.max_flush_time {
            self.max_flush_time = t_flush;
        }
        if t_flush < self.min_flush_time {
            self.min_flush_time = t_flush;
        }
        if did_reset {
            self.reset_count += 1;
        }
    }
}

// - usb descriptors ----------------------------------------------------------

use moondancer::usb::{DEVICE_SERIAL_STRING, DEVICE_VERSION_NUMBER};

static USB_DEVICE_DESCRIPTOR: DeviceDescriptor = DeviceDescriptor {
    descriptor_version: 0x0200,
    device_class: 0x00,
    device_subclass: 0x00,
    device_protocol: 0x00,
    max_packet_size: 64,
    vendor_id: cynthion::shared::usb::bVendorId::example,
    product_id: cynthion::shared::usb::bProductId::example,
    device_version_number: DEVICE_VERSION_NUMBER,
    manufacturer_string_index: 1,
    product_string_index: 2,
    serial_string_index: 3,
    num_configurations: 1,
    ..DeviceDescriptor::new()
};

static USB_DEVICE_QUALIFIER_DESCRIPTOR: DeviceQualifierDescriptor = DeviceQualifierDescriptor {
    descriptor_version: 0x0200,
    device_class: 0x00,
    device_subclass: 0x00,
    device_protocol: 0x00,
    max_packet_size: 64,
    num_configurations: 1,
    reserved: 0,
    ..DeviceQualifierDescriptor::new()
};

static USB_CONFIGURATION_DESCRIPTOR_0: ConfigurationDescriptor = ConfigurationDescriptor::new(
    ConfigurationDescriptorHeader {
        configuration_value: 1,
        configuration_string_index: 1,
        attributes: 0x80, // 0b1000_0000 = bus-powered
        max_power: 50,    // 50 * 2 mA = 100 mA
        ..ConfigurationDescriptorHeader::new()
    },
    &[InterfaceDescriptor::new(
        InterfaceDescriptorHeader {
            interface_number: 0,
            alternate_setting: 0,
            interface_class: 0x00,
            interface_subclass: 0x00,
            interface_protocol: 0x00,
            interface_string_index: 2,
            ..InterfaceDescriptorHeader::new()
        },
        &[
            EndpointDescriptor {
                endpoint_address: 0x01, // OUT
                attributes: 0x02,       // Bulk
                max_packet_size: 512,
                interval: 0,
                ..EndpointDescriptor::new()
            },
            EndpointDescriptor {
                endpoint_address: 0x02, // OUT - host commands
                attributes: 0x02,       // Bulk
                max_packet_size: 8,
                interval: 0,
                ..EndpointDescriptor::new()
            },
            EndpointDescriptor {
                endpoint_address: 0x81, // IN
                attributes: 0x02,       // Bulk
                max_packet_size: 512,
                interval: 0,
                ..EndpointDescriptor::new()
            },
        ],
    )],
);

static USB_OTHER_SPEED_CONFIGURATION_DESCRIPTOR_0: ConfigurationDescriptor =
    ConfigurationDescriptor::new(
        ConfigurationDescriptorHeader {
            descriptor_type: DescriptorType::OtherSpeedConfiguration as u8,
            configuration_value: 1,
            configuration_string_index: 1,
            attributes: 0x80, // 0b1000_0000 = bus-powered
            max_power: 50,    // 50 * 2 mA = 100 mA
            ..ConfigurationDescriptorHeader::new()
        },
        &[InterfaceDescriptor::new(
            InterfaceDescriptorHeader {
                interface_number: 0,
                alternate_setting: 0,
                interface_class: 0x00,
                interface_subclass: 0x00,
                interface_protocol: 0x00,
                interface_string_index: 2,
                ..InterfaceDescriptorHeader::new()
            },
            &[
                EndpointDescriptor {
                    endpoint_address: 0x01, // OUT
                    attributes: 0x02,       // Bulk
                    max_packet_size: 64,
                    interval: 0,
                    ..EndpointDescriptor::new()
                },
                EndpointDescriptor {
                    endpoint_address: 0x02, // OUT - host commands
                    attributes: 0x02,       // Bulk
                    max_packet_size: 8,
                    interval: 0,
                    ..EndpointDescriptor::new()
                },
                EndpointDescriptor {
                    endpoint_address: 0x81, // IN
                    attributes: 0x02,       // Bulk
                    max_packet_size: 64,
                    interval: 0,
                    ..EndpointDescriptor::new()
                },
            ],
        )],
    );

static USB_STRING_DESCRIPTOR_0: StringDescriptorZero =
    StringDescriptorZero::new(&[LanguageId::EnglishUnitedStates]);
static USB_STRING_DESCRIPTOR_1: StringDescriptor =
    StringDescriptor::new(cynthion::shared::usb::bManufacturerString::bulk_speed_test); // manufacturer
static USB_STRING_DESCRIPTOR_2: StringDescriptor =
    StringDescriptor::new(cynthion::shared::usb::bProductString::bulk_speed_test); // product
static USB_STRING_DESCRIPTOR_3: StringDescriptor = StringDescriptor::new(DEVICE_SERIAL_STRING); // serial

static USB_STRING_DESCRIPTORS: &[&StringDescriptor] = &[
    &USB_STRING_DESCRIPTOR_1,
    &USB_STRING_DESCRIPTOR_2,
    &USB_STRING_DESCRIPTOR_3,
];
