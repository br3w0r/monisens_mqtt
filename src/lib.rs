mod bindings_gen;
mod c_parser;

use bindings_gen as bg;
use c_parser::str_from_c_char;

use libc::c_void;
use regex::Regex;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use std::collections::HashSet;
use std::ffi::c_char;
use std::fs::File;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::SystemTime;
use std::{
    ffi::{CStr, CString},
    ptr::null,
};
use tokio::runtime::{Builder, Handle as AsyncHandle};

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::mpsc::{channel, Receiver, Sender},
    // io::{ReadExt, WriteExt},
    task,
};

const CONN_CONF_FILE_NAME: &str = "conn_conf.bin";
const DEVICE_CONF_FILE_NAME: &str = "device_conf.bin";

const CONN_PARAM_IP: &str = "IP Address";
const CONN_PARAM_PORT: &str = "Port";
const CONN_PARAM_CLIENT_ID: &str = "Client ID";
const CONN_PARAM_USERNAME: &str = "Username";
const CONN_PARAM_PASSWORD: &str = "Password";

const DEV_CONF_ID_TOPIC: i32 = 1;
const DEV_CONF_ID_QOS: i32 = 2;

const SENSOR_NAME: &str = "light_temp_humidity";

const SENSOR_DATA_LIGHT: &str = "light";
const SENSOR_DATA_TEMP: &str = "temperature";
const SENSOR_DATA_HUMIDITY: &str = "humidity";
const SENSOR_DATA_TIMESTAMP: &str = "timestamp";

lazy_static! {
    static ref AVAILABLE_MSG_TYPES: HashSet<u8> = HashSet::from([b':', b'>']);
    static ref RE_IP: Regex = Regex::new(r"^(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)\.(25[0-5]|2[0-4][0-9]|[01]?[0-9][0-9]?)$").unwrap();
}

#[no_mangle]
pub extern "C" fn mod_version() -> u8 {
    1
}

pub struct Module {
    runtime: tokio::runtime::Runtime,

    data_dir: String,
    client: Option<Arc<Mutex<AsyncClient>>>,
    eventloop: Option<Arc<Mutex<EventLoop>>>,

    conn_conf: Option<ConnConf>,
    device_conf: Option<DeviceConf>,

    // Process flow
    thread_handle: Option<JoinHandle<()>>,
    stop_tx: Option<Sender<()>>,
}

impl Module {
    pub fn is_ready_for_start(&self) -> bool {
        !self.client.is_none() && !self.eventloop.is_none() && !self.conn_conf.is_none()
    }
}

#[repr(transparent)]
pub struct Handle(*mut c_void);

impl Handle {
    /// # Panics
    /// Panics if `self.0` == null.
    pub unsafe fn as_module(&self) -> &'static mut Module {
        let ptr = self.0 as *mut Module;
        ptr.as_mut().unwrap() // Expect null checks before
    }

    /// # Safety
    /// `self.0` != null.
    pub unsafe fn destroy(&mut self) {
        let ptr = self.0 as *mut Module;
        let _ = Box::from_raw(ptr);
        self.0 = std::ptr::null::<c_void>() as *mut _;
    }

    pub fn from_module(module: Module) -> Self {
        let reference = Box::leak(Box::new(module));
        Self((reference as *mut Module) as _)
    }
}

#[no_mangle]
pub unsafe extern "C" fn functions() -> bg::Functions {
    bg::Functions {
        init: Some(init),
        obtain_device_info: Some(obtain_device_info),
        destroy: Some(destroy),
        connect_device: Some(connect_device),
        obtain_device_conf_info: Some(obtain_device_conf_info),
        configure_device: Some(configure_device),
        obtain_sensor_type_infos: Some(obtain_sensor_type_infos),
        start: Some(start),
        stop: Some(stop),
    }
}

#[no_mangle]
pub unsafe extern "C" fn init(sel: *mut *mut c_void, data_dir: *mut c_char) {
    let runtime = Builder::new_multi_thread()
        .thread_stack_size(128 * 1024)
        .enable_all()
        .build()
        .unwrap();

    let data_dir = str_from_c_char(data_dir);

    let m = if let Ok(conn_conf_file) = File::open(&(data_dir.clone() + CONN_CONF_FILE_NAME)) {
        // Reinitialize already initialized module
        let device_conf_file = File::open(&(data_dir.clone() + DEVICE_CONF_FILE_NAME))
            .expect("couldn't find device conf file. Was module fully initialized?");

        let conn_conf: ConnConf = bincode::deserialize_from(&conn_conf_file).expect(&format!(
            "failed to deserialize conn config file: '{data_dir}'"
        ));

        let device_conf: DeviceConf = bincode::deserialize_from(&device_conf_file).expect(
            &format!("failed to deserialize device config file: '{data_dir}'"),
        );

        let (client, eventloop) =
            connect_client(runtime.handle(), &conn_conf).expect("failed to connect to client");

        Module {
            runtime,
            data_dir,
            client: Some(Arc::new(Mutex::new(client))),
            eventloop: Some(Arc::new(Mutex::new(eventloop))),
            conn_conf: Some(conn_conf),
            device_conf: Some(device_conf),
            thread_handle: None,
            stop_tx: None,
        }
    } else {
        // Initialize new module
        Module {
            runtime,
            data_dir,
            client: None,
            eventloop: None,
            conn_conf: None,
            device_conf: None,
            thread_handle: None,
            stop_tx: None,
        }
    };

    *sel = Handle::from_module(m).0;
}

#[no_mangle]
pub unsafe extern "C" fn obtain_device_info(
    _: *mut c_void,
    obj: *mut c_void,
    callback: bg::device_info_callback,
) {
    let port_param_name = CString::new(CONN_PARAM_IP).unwrap();
    let port_param_port = CString::new(CONN_PARAM_PORT).unwrap();
    let port_param_client_id = CString::new(CONN_PARAM_CLIENT_ID).unwrap();
    let port_param_username = CString::new(CONN_PARAM_USERNAME).unwrap();
    let port_param_password = CString::new(CONN_PARAM_PASSWORD).unwrap();

    let params_vec: Vec<bg::ConnParamInfo> = vec![
        bg::ConnParamInfo {
            name: port_param_name.as_ptr() as _,
            typ: bg::ConnParamType::ConnParamString,
            info: null::<c_void>() as _,
        },
        bg::ConnParamInfo {
            name: port_param_port.as_ptr() as _,
            typ: bg::ConnParamType::ConnParamInt,
            info: null::<c_void>() as _,
        },
        bg::ConnParamInfo {
            name: port_param_client_id.as_ptr() as _,
            typ: bg::ConnParamType::ConnParamString,
            info: null::<c_void>() as _,
        },
        bg::ConnParamInfo {
            name: port_param_username.as_ptr() as _,
            typ: bg::ConnParamType::ConnParamString,
            info: null::<c_void>() as _,
        },
        bg::ConnParamInfo {
            name: port_param_password.as_ptr() as _,
            typ: bg::ConnParamType::ConnParamString,
            info: null::<c_void>() as _,
        },
    ];
    let mut conn_info = bg::DeviceConnectInfo {
        connection_params: params_vec.as_ptr() as _,
        connection_params_len: params_vec.len() as _,
    };

    callback.unwrap()(obj, &mut conn_info as _);
}

#[no_mangle]
pub unsafe extern "C" fn destroy(sel: *mut c_void) {
    stop(sel);
    Handle(sel).destroy();
}

const DEVICE_ERROR_NONE: u8 = 0;

#[derive(Debug)]
#[repr(u8)]
pub enum DeviceErr {
    DeviceErrConn = 1,
    DeviceErrParams = 2,
}

#[no_mangle]
pub extern "C" fn connect_device(handler: *mut c_void, confs: *mut bg::DeviceConnectConf) -> u8 {
    if let Err(err) = connect_device_impl(handler, confs) {
        err as _
    } else {
        DEVICE_ERROR_NONE
    }
}

fn connect_device_impl(
    handler: *mut c_void,
    confs: *mut bg::DeviceConnectConf,
) -> Result<(), DeviceErr> {
    let module = unsafe { Handle(handler).as_module() };
    let conf = ConnConf::new(confs)?;

    let (client, eventloop) = connect_client(module.runtime.handle(), &conf)?;

    // Save new conf to file
    let conf_ser = bincode::serialize(&conf).expect("failed to serialize new conn conf");
    let mut file = File::create(&(module.data_dir.clone() + CONN_CONF_FILE_NAME))
        .expect("failed to create or open conf file for saving new conn conf");
    file.write_all(&conf_ser)
        .expect("failed to write new conn conf into file");

    module.client = Some(Arc::new(Mutex::new(client)));
    module.eventloop = Some(Arc::new(Mutex::new(eventloop)));
    module.conn_conf = Some(conf);

    Ok(())
}

extern "C" fn obtain_device_conf_info(
    _: *mut c_void,
    obj: *mut c_void,
    callback: bg::device_conf_info_callback,
) {
    let mut entries = Vec::with_capacity(2);

    // ENTRY: MQTT topic
    let entry_topic_name = CString::new("MQTT topic").unwrap();
    let mut entry_topic_min_len = 1i32;
    let mut entry_topic_max_len = 65535i32;
    let mut entry_topic = bg::DeviceConfInfoEntryString {
        required: true,
        def: null::<c_void>() as _,
        min_len: &mut entry_topic_min_len as _,
        max_len: &mut entry_topic_max_len as _,
        match_regex: null::<c_void>() as _,
    };
    entries.push(bg::DeviceConfInfoEntry {
        id: DEV_CONF_ID_TOPIC,
        name: entry_topic_name.as_ptr() as _,
        typ: bg::DeviceConfInfoEntryType::DeviceConfInfoEntryTypeString,
        data: &mut entry_topic as *mut bg::DeviceConfInfoEntryString as *mut c_void,
    });

    // ENTRY: MQTT QoS
    let entry_qos_name = CString::new("MQTT QoS").unwrap();
    let mut entry_qos_lt = 3i32;
    let mut entry_qos_gt = -1i32;
    let mut entry_qos_def = 0;
    let mut entry_qos = bg::DeviceConfInfoEntryInt {
        required: true,
        def: &mut entry_qos_def as _,
        lt: &mut entry_qos_lt as _,
        gt: &mut entry_qos_gt as _,
        neq: null::<c_void>() as _,
    };
    entries.push(bg::DeviceConfInfoEntry {
        id: DEV_CONF_ID_QOS,
        name: entry_qos_name.as_ptr() as _,
        typ: bg::DeviceConfInfoEntryType::DeviceConfInfoEntryTypeInt,
        data: &mut entry_qos as *mut bg::DeviceConfInfoEntryInt as *mut c_void,
    });

    let mut conf_info = bg::DeviceConfInfo {
        device_confs: entries.as_ptr() as _,
        device_confs_len: entries.len() as _,
    };

    unsafe { callback.unwrap()(obj, &mut conf_info as _) };
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct ConnConf {
    ip: String,
    port: u16,
    client_id: String,
    username: String,
    password: String,
}

fn qos_from_u8(qos: u8) -> Option<QoS> {
    match qos {
        0 => Some(QoS::AtMostOnce),
        1 => Some(QoS::AtLeastOnce),
        2 => Some(QoS::ExactlyOnce),
        _ => None,
    }
}

impl ConnConf {
    fn new(confs_raw: *mut bg::DeviceConnectConf) -> Result<Self, DeviceErr> {
        if confs_raw.is_null() {
            return Err(DeviceErr::DeviceErrParams);
        }

        let confs = unsafe {
            std::slice::from_raw_parts(
                (*confs_raw).connection_params,
                (*confs_raw).connection_params_len as usize,
            )
        };

        let mut res_conf = ConnConf::default();

        for conf in confs {
            if let Ok(name) = unsafe { CStr::from_ptr(conf.name) }.to_str() {
                match name {
                    CONN_PARAM_IP => {
                        if let Some(ip) = c_parser::as_string(conf.value) {
                            res_conf.ip = ip;
                        } else {
                            return Err(DeviceErr::DeviceErrParams);
                        }
                    }
                    CONN_PARAM_PORT => {
                        if let Some(port) = c_parser::as_from_str::<u16>(conf.value) {
                            res_conf.port = port;
                        } else {
                            return Err(DeviceErr::DeviceErrParams);
                        }
                    }
                    CONN_PARAM_CLIENT_ID => {
                        if let Some(client_id) = c_parser::as_string(conf.value) {
                            res_conf.client_id = client_id;
                        } else {
                            return Err(DeviceErr::DeviceErrParams);
                        }
                    }
                    CONN_PARAM_USERNAME => {
                        if let Some(username) = c_parser::as_string(conf.value) {
                            res_conf.username = username;
                        } else {
                            return Err(DeviceErr::DeviceErrParams);
                        }
                    }
                    CONN_PARAM_PASSWORD => {
                        if let Some(password) = c_parser::as_string(conf.value) {
                            res_conf.password = password;
                        } else {
                            return Err(DeviceErr::DeviceErrParams);
                        }
                    }
                    _ => {
                        return Err(DeviceErr::DeviceErrParams);
                    }
                }
            } else {
                return Err(DeviceErr::DeviceErrParams);
            }
        }

        if !res_conf.validate() {
            return Err(DeviceErr::DeviceErrParams);
        }

        Ok(res_conf)
    }

    fn validate(&self) -> bool {
        if !RE_IP.is_match(&self.ip) {
            return false;
        }

        if self.port <= 0 {
            return false;
        }

        true
    }
}

#[derive(Default, Debug, Serialize, Deserialize)]
struct DeviceConf {
    topic: String,
    qos: u8,
}

impl DeviceConf {
    pub fn new(raw: *mut bg::DeviceConf) -> Result<DeviceConf, DeviceErr> {
        let raw_conf = unsafe { std::slice::from_raw_parts((*raw).confs, (*raw).confs_len as _) };

        let mut res_conf = DeviceConf::default();

        for conf in raw_conf {
            match conf.id {
                DEV_CONF_ID_TOPIC => {
                    let text = conf.data as *const c_char;
                    res_conf.topic = c_parser::str_from_c_char(text)
                }
                DEV_CONF_ID_QOS => {
                    let raw_qos = unsafe { *(conf.data as *const i32) };
                    if raw_qos < 0 || raw_qos > 2 {
                        return Err(DeviceErr::DeviceErrParams);
                    }

                    res_conf.qos = raw_qos as u8;
                }
                _ => {
                    return Err(DeviceErr::DeviceErrParams);
                }
            }
        }

        Ok(res_conf)
    }
}

extern "C" fn configure_device(handler: *mut c_void, conf: *mut bg::DeviceConf) -> u8 {
    if let Err(err) = configure_device_impl(handler, conf) {
        err as _
    } else {
        DEVICE_ERROR_NONE
    }
}

fn configure_device_impl(handler: *mut c_void, conf: *mut bg::DeviceConf) -> Result<(), DeviceErr> {
    let device_conf = DeviceConf::new(conf)?;
    let module = unsafe { Handle(handler).as_module() };

    let mut client = module
        .client
        .as_ref()
        .ok_or(DeviceErr::DeviceErrConn)?
        .lock()
        .unwrap();

    // Save new conf to file
    let conf_ser = bincode::serialize(&device_conf).expect("failed to serialize new device conf");
    let mut file = File::create(&(module.data_dir.clone() + DEVICE_CONF_FILE_NAME))
        .expect("failed to create or open conf file for saving new device conf");
    file.write_all(&conf_ser)
        .expect("failed to write new device conf into file");

    module.device_conf = Some(device_conf);

    Ok(())
}

extern "C" fn obtain_sensor_type_infos(
    _: *mut c_void,
    obj: *mut c_void,
    callback: bg::sensor_type_infos_callback,
) -> u8 {
    // SENSOR: Test Server
    let type_info_light_name = CString::new(SENSOR_DATA_LIGHT).unwrap();
    let type_info_light = bg::SensorDataTypeInfo {
        name: type_info_light_name.as_ptr() as _,
        typ: bg::SensorDataType::SensorDataTypeInt16,
    };

    let type_info_temp_name = CString::new(SENSOR_DATA_TEMP).unwrap();
    let type_info_temp = bg::SensorDataTypeInfo {
        name: type_info_temp_name.as_ptr() as _,
        typ: bg::SensorDataType::SensorDataTypeFloat32,
    };

    let type_info_humidity_name = CString::new(SENSOR_DATA_HUMIDITY).unwrap();
    let type_info_humidity = bg::SensorDataTypeInfo {
        name: type_info_humidity_name.as_ptr() as _,
        typ: bg::SensorDataType::SensorDataTypeFloat32,
    };

    let type_info_timestamp_name = CString::new(SENSOR_DATA_TIMESTAMP).unwrap();
    let type_info_timestamp = bg::SensorDataTypeInfo {
        name: type_info_timestamp_name.as_ptr() as _,
        typ: bg::SensorDataType::SensorDataTypeTimestamp,
    };

    let sensor_type_info_vec = vec![
        type_info_light,
        type_info_temp,
        type_info_humidity,
        type_info_timestamp,
    ];

    let sensor_name = CString::new(SENSOR_NAME).unwrap();

    // Sensor infos
    let sensor_type_infos_vec = vec![bg::SensorTypeInfo {
        name: sensor_name.as_ptr() as _,
        data_type_infos_len: sensor_type_info_vec.len() as _,
        data_type_infos: sensor_type_info_vec.as_ptr() as _,
    }];

    let sensor_type_infos = bg::SensorTypeInfos {
        sensor_type_infos_len: sensor_type_infos_vec.len() as _,
        sensor_type_infos: sensor_type_infos_vec.as_ptr() as _,
    };

    unsafe { callback.unwrap()(obj, &sensor_type_infos as *const _ as *mut _) };

    DEVICE_ERROR_NONE
}

struct MsgHandle(*mut c_void);

unsafe impl Send for MsgHandle {}

extern "C" fn start(
    handler: *mut c_void,
    msg_handler: *mut c_void,
    handle_func: bg::handle_msg_func,
) -> u8 {
    if let Err(err) = start_impl(handler, msg_handler, handle_func) {
        err as _
    } else {
        DEVICE_ERROR_NONE
    }
}

fn start_impl(
    handler: *mut c_void,
    msg_handler: *mut c_void,
    handle_func: bg::handle_msg_func,
) -> Result<(), DeviceErr> {
    let module = unsafe { Handle(handler).as_module() };

    if !module.is_ready_for_start() {
        panic!("start function when the module is not ready to for start");
    }

    let handle = MsgHandle(msg_handler);
    let (tx, rx) = channel(1);

    let client = module
        .client
        .as_ref()
        .ok_or(DeviceErr::DeviceErrConn)?
        .clone();
    let eventloop = module
        .eventloop
        .as_ref()
        .ok_or(DeviceErr::DeviceErrConn)?
        .clone();

    let topic = module.device_conf.as_ref().unwrap().topic.clone();
    let qos = module.device_conf.as_ref().unwrap().qos.clone();

    let async_handle = module.runtime.handle();

    let t = thread::spawn(move || {
        let msg_processor = MsgProcessor {
            handle_func,
            handle,
        };

        {
            let client = client.lock().unwrap();
            async_handle.block_on(client.subscribe(
                topic,
                qos_from_u8(qos).expect(&format!("incorrect qos: {qos}")),
            )).expect("failed to subscribe");
        }

        let mut stop_rx = rx;
        let mut eventloop = eventloop.lock().unwrap();

        while let Some(res) = async_handle.block_on(async {
            tokio::select! {
                _ = stop_rx.recv() => {
                    return None
                }
                res = eventloop.poll() => {
                    Some(res)
                }
            }
        }) {
            if let Ok(event) = res {
                if let Event::Incoming(ev) = event {
                    if let Packet::Publish(p) = ev {
                        msg_processor.process(p.payload.to_vec());
                    }
                }
            } else {
                println!("MQTT client error: {res:?}")
            }
        }
    });

    module.thread_handle = Some(t);
    module.stop_tx = Some(tx);

    Ok(())
}

extern "C" fn stop(handler: *mut ::std::os::raw::c_void) -> u8 {
    let module = unsafe { Handle(handler).as_module() };

    let opt_handle = module.thread_handle.take();
    if let Some(handle) = opt_handle {
        module.stop_tx.take().unwrap().blocking_send(()).unwrap();
        handle.join().unwrap();
    }

    DEVICE_ERROR_NONE
}

pub struct SensorData {
    light: i16,
    temp: f32,
    humidity: f32,
}

struct MsgProcessor {
    handle_func: bg::handle_msg_func,
    handle: MsgHandle,
}

impl MsgProcessor {
    fn process(&self, msg: Vec<u8>) {
        let sanitized_msg = Self::sanitize_serial_msg(msg);
        if sanitized_msg.is_none() {
            return;
        }
        let sanitized_msg = sanitized_msg.unwrap();

        let mut msg_iter = sanitized_msg.chars();
        let typ = msg_iter.next().expect("msg from device is empty");
        let msg: String = msg_iter.collect();

        match typ {
            ':' => {
                let sensor_data = Self::format_data_msg(&msg).unwrap();
                self.send_sensor_data(sensor_data);
            }
            _ => panic!("unexpected message from device: {msg}"),
        };
    }

    fn send_sensor_data(&self, data: SensorData) {
        let sensor_timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let type_info_light_name = CString::new(SENSOR_DATA_LIGHT).unwrap();
        let type_info_temp_name = CString::new(SENSOR_DATA_TEMP).unwrap();
        let type_info_humidity_name = CString::new(SENSOR_DATA_HUMIDITY).unwrap();
        let type_info_timestamp_name = CString::new(SENSOR_DATA_TIMESTAMP).unwrap();

        let data = vec![
            bg::SensorMsgData {
                name: type_info_light_name.as_ptr() as _,
                typ: bg::SensorDataType::SensorDataTypeInt16,
                data: &data.light as *const i16 as _,
            },
            bg::SensorMsgData {
                name: type_info_temp_name.as_ptr() as _,
                typ: bg::SensorDataType::SensorDataTypeFloat32,
                data: &data.temp as *const f32 as _,
            },
            bg::SensorMsgData {
                name: type_info_humidity_name.as_ptr() as _,
                typ: bg::SensorDataType::SensorDataTypeFloat32,
                data: &data.humidity as *const f32 as _,
            },
            bg::SensorMsgData {
                name: type_info_timestamp_name.as_ptr() as _,
                typ: bg::SensorDataType::SensorDataTypeTimestamp,
                data: &sensor_timestamp as *const i64 as *mut _,
            },
        ];

        let sensor_name = CString::new(SENSOR_NAME).unwrap();
        let msg = bg::SensorMsg {
            name: sensor_name.as_ptr() as _,
            data: data.as_ptr() as *mut _,
            data_len: data.len() as _,
        };

        let msg_data = bg::Message {
            typ: bg::MessageType::MessageTypeSensor,
            data: &msg as *const bg::SensorMsg as *mut _,
        };

        unsafe { self.handle_func.unwrap()(self.handle.0, msg_data) };
    }

    fn format_data_msg(data: &str) -> Result<SensorData, ()> {
        let parts: Vec<&str> = data.split(';').collect();

        if parts.len() < 3 {
            return Err(());
        }

        let light: i16 = if let Ok(val) = parts[0].parse() {
            Ok(val)
        } else {
            Err(())
        }?;

        let temp: f32 = if let Ok(val) = parts[1].parse() {
            Ok(val)
        } else {
            Err(())
        }?;

        let humidity: f32 = if let Ok(val) = parts[2].parse() {
            Ok(val)
        } else {
            Err(())
        }?;

        Ok(SensorData {
            light,
            temp,
            humidity,
        })
    }

    fn sanitize_serial_msg(mut msg: Vec<u8>) -> Option<String> {
        for (i, ch) in msg.iter().enumerate() {
            if AVAILABLE_MSG_TYPES.contains(ch) {
                return String::from_utf8(msg.drain(i..).collect()).ok();
            }
        }

        None
    }
}

fn connect_client(
    async_handle: &AsyncHandle,
    conf: &ConnConf,
) -> Result<(AsyncClient, EventLoop), DeviceErr> {
    let mut opts = MqttOptions::new(conf.client_id.clone(), conf.ip.clone(), conf.port);
    opts.set_credentials(conf.username.clone(), conf.password.clone());

    let (client, mut eventloop) = AsyncClient::new(opts, 5);

    async_handle.block_on(async {
        let res = eventloop.poll().await;

        if res.is_err() {
            Err(DeviceErr::DeviceErrConn)
        } else {
            Ok(())
        }
    })?;

    Ok((client, eventloop))
}
