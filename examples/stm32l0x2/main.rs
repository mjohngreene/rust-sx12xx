#![cfg_attr(not(test), no_std)]
#![no_main]

// To use example, press any key in serial terminal
// Packet will send and "Transmit Done!" will print when radio is done sending packet

extern crate nb;
extern crate panic_ramdump;

use core::fmt::Write;
use lorawan_crypto::LorawanCrypto as Crypto;
use lorawan_device::{
    radio, Device as LorawanDevice, Error as LorawanError, Event as LorawanEvent,
    Response as LorawanResponse,
};
use rtfm::app;
use stm32l0xx_hal::exti::{ExtiLine, GpioLine};
use stm32l0xx_hal::serial::Serial1Ext;
use stm32l0xx_hal::serial::USART1 as DebugUsart;
use stm32l0xx_hal::{exti::Exti, prelude::*, rcc, rng::Rng, syscfg, timer::Timer};
use stm32l0xx_hal::{pac, pac::Interrupt, rng, serial};
use sx12xx;
use sx12xx::{LorawanRadio, Sx12xx};
mod bindings;
pub use bindings::initialize_irq as initialize_radio_irq;
pub use bindings::RadioIRQ;
pub use bindings::TcxoEn;

static mut RNG: Option<rng::Rng> = None;
fn get_random_u32() -> u32 {
    unsafe {
        if let Some(rng) = &mut RNG {
            // enable starts the ADC conversions that generate the random number
            rng.enable();
            // wait until the flag flips; interrupt driven is possible but no implemented
            rng.wait();
            // reading the result clears the ready flag
            let val = rng.take_result();
            // can save some power by disabling until next random number needed
            rng.disable();
            val
        } else {
            panic!("No Rng exists!");
        }
    }
}

pub struct TimerContext {
    pub target: u16,
    pub count: u16,
    pub enable: bool,
    pub fired: bool,
}

#[app(device = stm32l0xx_hal::pac, peripherals = true)]
const APP: () = {
    struct Resources {
        int: Exti,
        radio_irq: RadioIRQ,
        debug_uart: serial::Tx<DebugUsart>,
        uart_rx: serial::Rx<DebugUsart>,
        timer: Timer<pac::TIM2>,
        #[init([0;512])]
        buffer: [u8; 512],
        #[init(0)]
        count: u8,
        #[init(false)]
        ready_to_send: bool,
        lorawan: Option<LorawanDevice<LorawanRadio, Crypto>>,
        #[init(TimerContext {
        target: 0,
        count: 0,
        enable: false,
        fired: false,
        })]
        timer_context: TimerContext,
    }

    #[init(spawn = [send_ping, lorawan_event], resources = [buffer])]
    fn init(ctx: init::Context) -> init::LateResources {
        let device = ctx.device;
        let mut rcc = device.RCC.freeze(rcc::Config::hsi16());
        let mut syscfg = syscfg::SYSCFG::new(device.SYSCFG, &mut rcc);
        let gpioa = device.GPIOA.split(&mut rcc);
        let gpiob = device.GPIOB.split(&mut rcc);
        let gpioc = device.GPIOC.split(&mut rcc);

        let (tx_pin, rx_pin, serial_peripheral) = (gpioa.pa9, gpioa.pa10, device.USART1);
        //let (tx_pin, rx_pin, serial_peripheral) = (gpioa.pa2, gpioa.pa3, device.USART2);

        let mut serial = serial_peripheral
            .usart(tx_pin, rx_pin, serial::Config::default(), &mut rcc)
            .unwrap();

        // listen for incoming bytes which will trigger transmits
        serial.listen(serial::Event::Rxne);
        let (mut tx, rx) = serial.split();

        write!(tx, "LongFi Device Test\r\n").unwrap();

        let mut exti = Exti::new(device.EXTI);

        // constructor initializes 48 MHz clock that RNG requires
        // Initialize 48 MHz clock and RNG
        let hsi48 = rcc.enable_hsi48(&mut syscfg, device.CRS);
        unsafe { RNG = Some(Rng::new(device.RNG, &mut rcc, hsi48)) };
        let radio_irq = initialize_radio_irq(gpiob.pb4, &mut syscfg, &mut exti);

        // Configure the timer.
        let timer = device.TIM2.timer(1.khz(), &mut rcc);

        let bindings = bindings::new(
            device.SPI1,
            &mut rcc,
            gpiob.pb3,
            gpioa.pa6,
            gpioa.pa7,
            gpioa.pa15,
            gpioc.pc0,
            gpioa.pa1,
            gpioc.pc2,
            gpioc.pc1,
            Some(gpioa.pa8), // None //use pa8 for catena
        );

        let mut sx12xx = Sx12xx::new(sx12xx::Radio::sx1276(), bindings);
        sx12xx.set_public_network(true);

        let lorawan = LorawanDevice::new(
            LorawanRadio::new(sx12xx),
            [0x55, 0x6C, 0xB6, 0x1E, 0x37, 0xC5, 0x3C, 0x00],
            [0xB9, 0x94, 0x02, 0xD0, 0x7E, 0xD5, 0xB3, 0x70],
            [
                0xBF, 0x40, 0xD3, 0x0E, 0x4E, 0x23, 0x42, 0x8E, 0xF6, 0x82, 0xCA, 0x77, 0x64, 0xCD,
                0xB4, 0x23,
            ],
            get_random_u32,
        );

        ctx.spawn.lorawan_event(LorawanEvent::NewSession).unwrap();

        write!(tx, "Going to main loop\r\n").unwrap();

        // Return the initialised resources.
        init::LateResources {
            int: exti,
            radio_irq,
            debug_uart: tx,
            uart_rx: rx,
            lorawan: Some(lorawan),
            timer,
        }
    }

    #[task(capacity = 4, priority = 2, resources = [debug_uart, buffer, lorawan], spawn  = [lorawan_response])]
    fn lorawan_event(ctx: lorawan_event::Context, event: LorawanEvent<'static, LorawanRadio>) {
        let debug = ctx.resources.debug_uart;
        if let Some(lorawan) = ctx.resources.lorawan.take() {
            // debug statements for the event
            match &event {
                LorawanEvent::NewSession => {
                    write!(debug, "New Session Request \r\n").unwrap();
                }
                LorawanEvent::RadioEvent(e) => match e {
                    radio::Event::TxRequest(_, _) => (),
                    radio::Event::RxRequest(_) => (),
                    radio::Event::CancelRx => (),
                    radio::Event::PhyEvent(phy) => {
                        write!(debug, "RadioPhy ").unwrap();
                        let event = phy as &sx12xx::Event;
                        match event {
                            sx12xx::Event::DIO0(t) => write!(debug, "DIO0({})\r\n", t).unwrap(),
                            _ => write!(debug, "\r\n").unwrap(),
                        }
                    }
                },
                LorawanEvent::Timeout => (),
                LorawanEvent::SendData(_e) => {
                    write!(debug, "SendData \r\n").unwrap();
                }
            }
            let (new_state, response) = lorawan.handle_event(event);
            ctx.spawn.lorawan_response(response);
            *ctx.resources.lorawan = Some(new_state);
        }
    }

    #[task(capacity = 4, priority = 2, resources = [debug_uart, timer_context, lorawan])]
    fn lorawan_response(ctx: lorawan_response::Context, response: Result<LorawanResponse, LorawanError<LorawanRadio>>) {
        let debug = ctx.resources.debug_uart;

        match response {
            Ok(response) => {
                match response {
                    LorawanResponse::TimeoutRequest(ms) => {
                        let timer_context = ctx.resources.timer_context;
                        timer_context.target = ms as u16;
                        timer_context.fired = false;
                    }
                    LorawanResponse::NewSession => {
                        if let Some(mut lorawan) = ctx.resources.lorawan.take() {
                            write!(
                                debug,
                                "NewSession: {:?}\r\n",
                                lorawan.get_session_keys().unwrap()
                            )
                                .unwrap();

                            *ctx.resources.lorawan = Some(lorawan);
                        }
                        let timer_context = ctx.resources.timer_context;
                        timer_context.enable = false;
                    }
                    LorawanResponse::ReadyToSend => {
                        write!(debug, "ReadyToSend\r\n").unwrap();
                        let timer_context = ctx.resources.timer_context;
                        timer_context.enable = false;
                    }
                    _ => {
                        if let LorawanResponse::WaitingForJoinAccept = response {}
                        write!(debug, "Response: {:?}\r\n", response).unwrap();
                    }
                }
            }
            Err(err) => match err {
                LorawanError::Radio(_) => write!(debug, "Radio \r\n").unwrap(),
                LorawanError::Session(_) => write!(debug, "Session \r\n").unwrap(),
                LorawanError::NoSession(_) => write!(debug, "NoSession\r\n").unwrap(),
            },
        }
    }

    #[task(capacity = 4, priority = 2, resources = [debug_uart, count, lorawan])]
    fn send_ping(ctx: send_ping::Context) {
        if let Some(lorawan) = ctx.resources.lorawan.take() {
            let debug = ctx.resources.debug_uart;

            if lorawan.ready_to_send_data() {
                write!(debug, "Sending Ping\r\n").unwrap();
                *ctx.resources.count += 1;

                let data: [u8; 5] = [0xDE, 0xAD, 0xBE, 0xEF, *ctx.resources.count];
                let (new_state, response) = lorawan.send(&data, 1, false);
                *ctx.resources.lorawan = Some(new_state);
            } else {
                write!(debug, "Suppressing Send Request\r\n").unwrap();
            }
        }
    }

    //#[task(binds = USART2, priority=2, resources = [uart_rx], spawn = [send_ping])]
    //fn USART2(ctx: USART2::Context) {
    #[task(binds = USART1, priority=1, resources = [uart_rx], spawn = [send_ping])]
    fn USART1(ctx: USART1::Context) {
        let rx = ctx.resources.uart_rx;
        rx.read().unwrap();
        ctx.spawn.send_ping().unwrap();
    }

    #[task(binds = EXTI4_15, priority = 1, resources = [radio_irq, int, timer_context], spawn = [lorawan_event])]
    fn EXTI4_15(mut ctx: EXTI4_15::Context) {
        Exti::unpend(GpioLine::from_raw_line(ctx.resources.radio_irq.pin_number()).unwrap());
        let mut count = 0;
        // grab a lock on timer and start new
        ctx.resources.timer_context.lock(|context| {
            if context.enable {
                count = context.count as u32;
                context.enable = false;
            } else {
                context.target = 0xFFFF as u16;
                context.count = 0;
                context.enable = true;
            }
        });
        rtfm::pend(Interrupt::TIM2);

        ctx.spawn
            .lorawan_event(lorawan_device::Event::RadioEvent(
                lorawan_device::radio::Event::PhyEvent(sx12xx::Event::DIO0(count)),
            ))
            .unwrap();
    }

    // This is a pretty not scalable timeout implementation
    // but we can switch to RTFM timer queues later maybe
    #[task(binds = TIM2, priority = 1, resources = [timer, timer_context], spawn = [lorawan_event])]
    fn TIM2(mut ctx: TIM2::Context) {
        let timer = ctx.resources.timer;
        let spawn = ctx.spawn;
        timer.clear_irq();

        ctx.resources.timer_context.lock(|context| {
            // if timer has been disabled,
            // timeout has been disarmed
            if !context.enable {
                context.target = 0;
                context.count = 0;
                timer.unlisten();
            } else {
                // if count is 0, we are just setting up a timeout
                if context.count == 0 {
                    timer.reset();
                    timer.listen();
                }
                context.count += 1;

                // if we have a match, timer has fired
                if context.count >= context.target && !context.fired {
                    spawn.lorawan_event(LorawanEvent::Timeout).unwrap();
                    context.fired = true;
                }
            }
        });
    }

    // Interrupt handlers used to dispatch software tasks
    extern "C" {
        fn USART4_USART5();
    }
};
