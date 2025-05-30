//! Draft of ESP32S3 project.

//% CHIPS: esp32 esp32s3
//% FEATURES: embassy esp-hal/unstable

#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(adt_const_params)]
#![feature(impl_trait_in_assoc_type)]
extern crate alloc;

use crate::ota::{validate_current_ota_slot, run_with_ota};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, Ordering};
use embassy_executor::{task, Spawner};
use embassy_net::tcp::client::{TcpClient, TcpClientState};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, signal::Signal};
use embassy_time::{Duration, Timer};
#[allow(unused_imports)]
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, system::{CpuControl, Stack}, timer::{timg::TimerGroup, AnyTimer}
};
use esp_hal::rng::Rng;
use esp_hal::{rmt::Rmt, time::Rate};
use esp_hal_embassy::Executor;
use esp_println::println;

use static_cell::StaticCell;

mod neopixel;

mod second_core;
mod main_core;
mod http;
mod wifi;

mod web_server;
mod shared;


use second_core::control_led;
use main_core::enable_disable_led;
use wifi::connect as connect_to_wifi;
use crate::http::EmbassyHttpClient;

use web_server::web_task;
use picoserve::{
    make_static,
    AppBuilder, AppRouter,
};
use crate::web_server::AppProps;

mod ota;
use embedded_storage::{ReadStorage};
use esp_bootloader_esp_idf::ota::Slot;
use esp_storage::FlashStorage;
use esp_bootloader_esp_idf::partitions;
use ota::OtaImageState::Valid;

mod db;
use ekv::Database;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use crate::db::DbFlash;

const CONFIG_PARTITION_START: usize = 0xA10000;


static mut APP_CORE_STACK: Stack<8192> = Stack::new();
static CLIENT_STATE: StaticCell<TcpClientState<3, 1024, 1024>> = StaticCell::new();
static TCP_CLIENT: StaticCell<TcpClient<'static, 3>> = StaticCell::new();


static CLIENT_STATE2: StaticCell<TcpClientState<3, 1024, 1024>> = StaticCell::new();
static TCP_CLIENT2: StaticCell<TcpClient<'static, 3>> = StaticCell::new();

pub static WIFI_INITIALIZED: AtomicBool = AtomicBool::new(false);
pub static WIFI_MODE_CLIENT: AtomicBool = AtomicBool::new(false);
pub static TIME_SYNCED: AtomicBool = AtomicBool::new(false);
pub static FIRMWARE_UPGRADE_IN_PROGRESS: AtomicBool = AtomicBool::new(false);


const fn or_str(opt: Option<&'static str>, default: &'static str) -> &'static str {
    if let Some(val) = opt {
        val
    } else if let None = opt {
        default
    } else {
        unreachable!()
    }
}

const SSID: &str = or_str(option_env!("SSID"), "MyDefaultSSID");
const PASSWORD: &str = or_str(option_env!("PASSWORD"), "MyDefaultPassword");

#[task(pool_size = 2)]
pub async fn http_wk(mut http_client: EmbassyHttpClient<'static,'static,3>, url: &'static str, period_ms: u64, name: &'static str,) {
    loop {
        println!("[{}] Running HTTP client", name);
        let _ = http_client.get(url, 2).await;
        Timer::after(Duration::from_millis(period_ms)).await;
    }
}


#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    esp_alloc::heap_allocator!(size: 72 * 1024);

    println!("****************** Storage Init ******************");
    let mut ota_flash = FlashStorage::new();
    println!("Flash size = {}", ota_flash.capacity());
    
    println!("****************** Peripherals Init ******************");
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    println!("****************** OTA Init ******************");
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    run_with_ota(&mut ota_flash, &mut pt_mem, |ota| {
        let current = ota.current_slot().unwrap();
        
        if current != Slot::None {
            ota.set_current_ota_state(Valid).unwrap(); 
        }
        println!("current OTA image state {:?}", ota.current_ota_state());
        println!("current OTA {:?} - next {:?}", current, current.next());

        validate_current_ota_slot(ota);
        
    }).unwrap();

    println!("****************** NeoPixel Init ******************");
    let led_pin = peripherals.GPIO48;
    let freq = Rate::from_mhz(80);
    let rmt = Rmt::new(peripherals.RMT, freq).unwrap();
    static LED_CTRL: StaticCell<Signal<CriticalSectionRawMutex, bool>> = StaticCell::new();
    let led_ctrl_signal = &*LED_CTRL.init(Signal::new());

    println!("****************** Timers Init ******************");
    let timer_g0 = TimerGroup::new(peripherals.TIMG0);
    let rng = Rng::new(peripherals.RNG);
    let timer_g1 = TimerGroup::new(peripherals.TIMG1);
    let timer0: AnyTimer = timer_g1.timer0.into();
    let timer1: AnyTimer = timer_g1.timer1.into();
    esp_hal_embassy::init([timer0, timer1]);

    let mut cpu_control = CpuControl::new(peripherals.CPU_CTRL);

    println!("****************** Led Worker Init ******************");
    let _guard = cpu_control
        .start_app_core(unsafe { &mut *addr_of_mut!(APP_CORE_STACK) }, move || {
            static EXECUTOR: StaticCell<Executor> = StaticCell::new();
            let executor = EXECUTOR.init(Executor::new());
            executor.run(|spawner| {
                spawner.spawn(control_led(led_pin, rmt, led_ctrl_signal)).ok();
            });
        })
        .unwrap();
    println!("****************** Led Control Init ******************");
    spawner.spawn(enable_disable_led(led_ctrl_signal)).unwrap();
    
    println!("****************** Wifi Init ******************");
    let stack = connect_to_wifi(spawner, timer_g0, rng, peripherals.WIFI, peripherals.RADIO_CLK, SSID,  PASSWORD).await.unwrap();
    WIFI_INITIALIZED.store(true, Ordering::Release);
    WIFI_MODE_CLIENT.store(false, Ordering::Release);

    println!(">>>>>>>>>>> System Init finished <<<<<<<<<<<<<<<<<<<<<,");
    
    let client_state = CLIENT_STATE.init(TcpClientState::new());
    let tcp_client = TCP_CLIENT.init(TcpClient::new(*stack, client_state));
    let http_client1 = EmbassyHttpClient::new(stack, tcp_client);

    let client_state2 = CLIENT_STATE2.init(TcpClientState::new());
    let tcp_client2 = TCP_CLIENT2.init(TcpClient::new(*stack, client_state2));
    let http_client2 = EmbassyHttpClient::new(stack, tcp_client2);

    
    println!(">>>>>>>>>>> HTTP Clients Init finished <<<<<<<<<<<<<<<<<<<<<<<<<<<<,");
 

    println!(">>>> Starting http worker 1");
    spawner.spawn(http_wk(http_client1, "http://mobile-j.de", 120000, "client1")).unwrap();
    Timer::after(Duration::from_secs(5)).await;
    println!(">>>> Starting http worker 2");
    spawner.spawn(http_wk(http_client2, "http://mobile-j.de", 120000, "client2")).unwrap();
    
    // Web server worker
    let sse_message_watch = web_server::init_sse_message_watch();
    let sse_message_sender = sse_message_watch.sender();
    let app = make_static!(AppRouter<AppProps>, AppProps.build_app());
    let config = make_static!(
        picoserve::Config<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );
    for id in 0..web_server::WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(id, *stack, app, config));
    }
    
    // Update SSE the message
    sse_message_sender.clear();
    sse_message_sender.send("Hello SSE!".parse().unwrap());

    println!("****************** DB Init ******************");
    let db_flash = DbFlash {
        flash: embassy_embedded_hal::adapter::BlockingAsync::new(FlashStorage::new()),
        start: CONFIG_PARTITION_START,
    };
    let db = Database::<_, NoopRawMutex>::new(db_flash, ekv::Config::default());

    if db.mount().await.is_err() {
        println!("Formatting...");
        db.format().await.unwrap();
    }

    let mut wtx = db.write_transaction().await;
    wtx.write(b"HELLO", b"WORLD").await.unwrap();
    wtx.commit().await.unwrap();

    let rtx = db.read_transaction().await;
    let mut buf = [0u8; 32];
    let hello = rtx.read(b"HELLO", &mut buf).await.map(|n| &buf[..n]).ok();
    if let Some(s) = hello {
        if let Ok(text) = core::str::from_utf8(s) {
            println!("HELLO: {}", text);
        } else {
            println!("HELLO (invalid UTF-8): {:?}", s);
        }
    }

    println!(">>>>>>>>>>> Init finished <<<<<<<<<<<<<<<<<<<<<,");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}
