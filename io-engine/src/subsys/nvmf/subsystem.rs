use std::{
    convert::TryFrom,
    ffi::{c_void, CString},
    fmt::{self, Debug, Display, Formatter},
    mem::zeroed,
    ptr::{self, NonNull},
};

use futures::channel::oneshot;
use nix::errno::Errno;

use spdk_rs::{
    libspdk::{
        nvmf_subsystem_find_listener,
        nvmf_subsystem_set_cntlid_range,
        spdk_nvmf_ctrlr_set_cpl_error_cb,
        spdk_nvmf_ns_get_bdev,
        spdk_nvmf_ns_opts,
        spdk_nvmf_request,
        spdk_nvmf_subsystem,
        spdk_nvmf_subsystem_add_host,
        spdk_nvmf_subsystem_add_listener,
        spdk_nvmf_subsystem_add_ns_ext,
        spdk_nvmf_subsystem_create,
        spdk_nvmf_subsystem_destroy,
        spdk_nvmf_subsystem_disconnect_host,
        spdk_nvmf_subsystem_event,
        spdk_nvmf_subsystem_get_first,
        spdk_nvmf_subsystem_get_first_host,
        spdk_nvmf_subsystem_get_first_listener,
        spdk_nvmf_subsystem_get_first_ns,
        spdk_nvmf_subsystem_get_next,
        spdk_nvmf_subsystem_get_next_host,
        spdk_nvmf_subsystem_get_next_listener,
        spdk_nvmf_subsystem_get_nqn,
        spdk_nvmf_subsystem_listener_get_trid,
        spdk_nvmf_subsystem_pause,
        spdk_nvmf_subsystem_remove_host,
        spdk_nvmf_subsystem_remove_ns,
        spdk_nvmf_subsystem_resume,
        spdk_nvmf_subsystem_set_allow_any_host,
        spdk_nvmf_subsystem_set_ana_reporting,
        spdk_nvmf_subsystem_set_ana_state,
        spdk_nvmf_subsystem_set_event_cb,
        spdk_nvmf_subsystem_set_mn,
        spdk_nvmf_subsystem_set_sn,
        spdk_nvmf_subsystem_start,
        spdk_nvmf_subsystem_state_change_done,
        spdk_nvmf_subsystem_stop,
        spdk_nvmf_tgt,
        SPDK_NVME_SCT_GENERIC,
        SPDK_NVME_SC_CAPACITY_EXCEEDED,
        SPDK_NVME_SC_RESERVATION_CONFLICT,
        SPDK_NVMF_SUBTYPE_DISCOVERY,
        SPDK_NVMF_SUBTYPE_NVME,
    },
    struct_size_init,
    NvmeStatus,
    NvmfController,
    NvmfSubsystemEvent,
};

use crate::{
    bdev::{nexus::NEXUS_MODULE_NAME, nvmx::NVME_CONTROLLERS, Nexus},
    constants::{NVME_CONTROLLER_MODEL_ID, NVME_NQN_PREFIX},
    core::{Bdev, Reactors, UntypedBdev},
    eventing::{host_events::HostTargetMeta, EventMetaGen, EventWithMeta},
    ffihelper::{cb_arg, done_cb, AsStr, FfiResult, IntoCString},
    lvs::Lvol,
    subsys::{
        make_subsystem_serial,
        nvmf::{transport::TransportId, Error, NVMF_TGT},
        Config,
    },
};
use events_api::event::EventAction;

/// TODO
#[derive(Debug, PartialOrd, PartialEq)]
pub enum SubType {
    Nvme,
    Discovery,
}

impl Display for SubType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match *self {
            SubType::Nvme => write!(f, "NVMe"),
            SubType::Discovery => write!(f, "Discovery"),
        }
    }
}

pub struct NvmfSubsystem(pub(crate) NonNull<spdk_nvmf_subsystem>);
pub struct NvmfSubsystemIterator(*mut spdk_nvmf_subsystem);

impl Iterator for NvmfSubsystemIterator {
    type Item = NvmfSubsystem;
    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_null() {
            None
        } else {
            let current = self.0;
            self.0 = unsafe { spdk_nvmf_subsystem_get_next(current) };
            NonNull::new(current).map(NvmfSubsystem)
        }
    }
}

impl IntoIterator for NvmfSubsystem {
    type Item = NvmfSubsystem;
    type IntoIter = NvmfSubsystemIterator;

    fn into_iter(self) -> Self::IntoIter {
        NVMF_TGT.with(|t| {
            NvmfSubsystemIterator(unsafe {
                spdk_nvmf_subsystem_get_first(t.borrow().tgt.as_ptr())
            })
        })
    }
}

impl Debug for NvmfSubsystem {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        unsafe {
            f.debug_struct("NvmfSubsystem")
                .field("id", &self.0.as_ref().id)
                .field("subtype", &self.subtype().to_string())
                .field("subnqn", &self.0.as_ref().subnqn.as_str().to_string())
                .field("sn", &self.0.as_ref().sn.as_str().to_string())
                .field("mn", &self.0.as_ref().mn.as_str().to_string())
                .field("allow_any_host", &self.0.as_ref().allow_any_host)
                .field("ana_reporting", &self.0.as_ref().flags.ana_reporting())
                .field("listeners", &self.listeners_to_vec())
                .finish()
        }
    }
}

impl From<*mut spdk_nvmf_subsystem> for NvmfSubsystem {
    fn from(s: *mut spdk_nvmf_subsystem) -> Self {
        NvmfSubsystem(NonNull::new(s).unwrap())
    }
}

impl NvmfSubsystem {
    /// TODO
    pub fn try_from_with<T>(
        bdev: &Bdev<T>,
        ptpl: Option<&std::path::PathBuf>,
    ) -> Result<Self, Error>
    where
        T: spdk_rs::BdevOps,
    {
        if bdev.is_claimed() {
            return Err(Error::CreateTarget {
                msg: "already shared".to_string(),
            });
        }
        let ss = NvmfSubsystem::new(bdev.name())?;
        ss.set_ana_reporting(false)?;
        ss.allow_any(false);
        if let Err(e) = ss.add_namespace(bdev, ptpl) {
            unsafe {
                ss.destroy_unsafe();
            }
            return Err(e);
        }
        Ok(ss)
    }
    /// TODO
    pub fn try_from<T>(bdev: &Bdev<T>) -> Result<Self, Error>
    where
        T: spdk_rs::BdevOps,
    {
        Self::try_from_with(bdev, None)
    }
}

fn make_sn<T: AsRef<[u8]>>(uuid: T) -> CString {
    let s = make_subsystem_serial(uuid);
    CString::new(s).unwrap()
}

impl NvmfSubsystem {
    /// callback function for reset controller operation
    fn reset_cb(success: bool, ctx: *mut c_void) {
        if success {
            info!(
                "nvmf_event_handler: controller successfully reset {:?}",
                success
            );
        } else {
            error!("nvmf_event_handler: failed to reset controller ");
        }
        done_cb(ctx, success);
    }

    /// Resets an NVMe controller.
    pub(crate) async fn reset_controller(dev_name: &str, ctx: *mut c_void) {
        let ctrlr = match NVME_CONTROLLERS.lookup_by_name(dev_name) {
            Some(ctrlr) => ctrlr,
            None => {
                debug!(
                    "Reset nexus child '{dev_name}': device name \
                    is not a valid NVMe controller"
                );
                Self::reset_cb(false, ctx);
                return;
            }
        };

        debug!("Reset nexus child '{dev_name}': NVMe controller {ctrlr:?}");

        let mut ctrlr = ctrlr.lock();

        match ctrlr.reset(NvmfSubsystem::reset_cb, ctx, false) {
            Ok(_) => {
                debug!(
                    "Reset nexus child '{dev_name}': \
                    reset initiated for {ctrlr:?}"
                );
            }
            Err(e) => {
                error!(
                    "Reset nexus child '{dev_name}': \
                    failed to initiate reset for {ctrlr:?}: {e}"
                );

                Self::reset_cb(false, ctx);
            }
        }
    }

    /// Subsystem event handlers.
    extern "C" fn nvmf_subsystem_event_handler(
        subsys: *mut spdk_nvmf_subsystem,
        event: spdk_nvmf_subsystem_event,
        ctx: *mut c_void,
        _cb_arg: *mut c_void,
    ) {
        let s = NvmfSubsystem::from(subsys);
        let event = NvmfSubsystemEvent::from_cb_args(event, ctx);

        debug!("NVMF subsystem event {s:?}: {event:?}");

        let nqn_tgt = NqnTarget::lookup(&s.get_nqn());
        if matches!(nqn_tgt, NqnTarget::None) {
            warn!(
                "NVMF subsystem event {s:?}: {event:?}: \
                target for event NQN not found"
            );
        }

        let event_meta = match nqn_tgt {
            NqnTarget::Nexus(n) => n.host_target_meta(s.meta()),
            NqnTarget::Replica(ref r) => r.host_target_meta(s.meta()),
            NqnTarget::None => s.meta(),
        };

        match event {
            NvmfSubsystemEvent::HostConnect(c) => {
                c.event(EventAction::NvmeConnect, event_meta).generate();

                match nqn_tgt {
                    NqnTarget::Nexus(n) => s.host_connect_nexus(c, n),
                    NqnTarget::Replica(r) => s.host_connect_replica(c, r),
                    NqnTarget::None => {}
                }
            }
            NvmfSubsystemEvent::HostDisconnect(c) => {
                c.event(EventAction::NvmeDisconnect, event_meta).generate();

                match nqn_tgt {
                    NqnTarget::Nexus(n) => s.host_disconnect_nexus(c, n),
                    NqnTarget::Replica(r) => s.host_disconnect_replica(c, r),
                    NqnTarget::None => {}
                }
            }
            NvmfSubsystemEvent::HostKeepAliveTimeout(c) => {
                c.event(EventAction::NvmeKeepAliveTimeout, event_meta)
                    .generate();

                match nqn_tgt {
                    NqnTarget::Nexus(n) => s.host_kato_nexus(c, n),
                    NqnTarget::Replica(r) => s.host_kato_replica(c, r),
                    NqnTarget::None => {}
                }
            }
            NvmfSubsystemEvent::Unknown => {} // ignore unknown events
        }
    }

    /// Completion error callback for nexuses.
    unsafe extern "C" fn nexus_cpl_error_cb(
        req: *mut spdk_nvmf_request,
        _cb_arg: *mut ::std::os::raw::c_void,
    ) {
        let req = &mut *req;
        let cpl = req.nvme_cpl_mut();
        let mut status = cpl.status();

        if status.crd() == 0 {
            return;
        }

        // Use CRD #2 for certain errors.
        match status.status() {
            NvmeStatus::Generic(SPDK_NVME_SC_RESERVATION_CONFLICT)
            | NvmeStatus::Generic(SPDK_NVME_SC_CAPACITY_EXCEEDED) => {
                status.set_crd(2);
            }
            _ => {}
        }

        cpl.set_status(status);
    }

    /// Called upon a host connection to a nexus.
    fn host_connect_nexus(&self, ctrlr: NvmfController, nex: &Nexus) {
        info!(
            "Host '{host}' connected to subsystem '{subsys}' on \
            nexus '{nex:?}'",
            host = ctrlr.hostnqn(),
            subsys = self.get_nqn(),
        );

        nex.add_initiator(&ctrlr.hostnqn());

        unsafe {
            spdk_nvmf_ctrlr_set_cpl_error_cb(
                ctrlr.0.as_ptr(),
                Some(Self::nexus_cpl_error_cb),
                std::ptr::null_mut(),
            );
        }
    }

    /// Called upon a host disconnection from a nexus.
    fn host_disconnect_nexus(&self, ctrlr: NvmfController, nex: &Nexus) {
        info!(
            "Host '{host}' disconnected from subsystem '{subsys}' on \
            nexus '{nex:?}'",
            host = ctrlr.hostnqn(),
            subsys = self.get_nqn(),
        );

        nex.rm_initiator(&ctrlr.hostnqn());

        unsafe {
            spdk_nvmf_ctrlr_set_cpl_error_cb(
                ctrlr.0.as_ptr(),
                None,
                std::ptr::null_mut(),
            );
        }
    }

    /// Called upon a host keep alive timeout (KATO) on a nexus.
    fn host_kato_nexus(&self, ctrlr: NvmfController, nex: &Nexus) {
        warn!(
            "Host '{host}': keep alive timeout on subsystem '{subsys}' on \
            nexus '{nex:?}'",
            host = ctrlr.hostnqn(),
            subsys = self.get_nqn(),
        );

        nex.initiator_keep_alive_timeout(&ctrlr.hostnqn());
    }

    /// Completion error callback for replicas.
    unsafe extern "C" fn replica_cpl_error_cb(
        req: *mut spdk_nvmf_request,
        _cb_arg: *mut ::std::os::raw::c_void,
    ) {
        let req = &mut *req;
        let cpl = req.nvme_cpl_mut();

        let mut status = cpl.status();

        // Change CRD for replica to 3.
        if status.crd() == 1 {
            status.set_crd(3);
        }

        // Correct vendor-specific ENOSPC error.
        if status.status().is_no_space() {
            status.set_sct(SPDK_NVME_SCT_GENERIC as u16);
            status.set_sc(SPDK_NVME_SC_CAPACITY_EXCEEDED as u16);
        }

        cpl.set_status(status);
    }

    /// Called upon a host connection to a replica.
    fn host_connect_replica(&self, ctrlr: NvmfController, lvol: Lvol) {
        info!(
            "Host '{host}' connected to subsystem '{subsys}' on \
            replica '{lvol:?}'",
            host = ctrlr.hostnqn(),
            subsys = self.get_nqn(),
        );

        unsafe {
            spdk_nvmf_ctrlr_set_cpl_error_cb(
                ctrlr.0.as_ptr(),
                Some(Self::replica_cpl_error_cb),
                std::ptr::null_mut(),
            );
        }
    }

    /// Called upon a host disconnection from a replica.
    fn host_disconnect_replica(&self, ctrlr: NvmfController, lvol: Lvol) {
        info!(
            "Host '{host}' disconnected from subsystem '{subsys}' on \
            replica '{lvol:?}'",
            host = ctrlr.hostnqn(),
            subsys = self.get_nqn(),
        );

        unsafe {
            spdk_nvmf_ctrlr_set_cpl_error_cb(
                ctrlr.0.as_ptr(),
                None,
                std::ptr::null_mut(),
            );
        }
    }

    /// Called upon a host keep alive timeout (KATO) on a replica.
    fn host_kato_replica(&self, ctrlr: NvmfController, lvol: Lvol) {
        warn!(
            "Host '{host}': keep alive timeout on subsystem '{subsys}' on \
            replica '{lvol:?}'",
            host = ctrlr.hostnqn(),
            subsys = self.get_nqn(),
        );
    }

    /// create a new subsystem where the NQN is based on the UUID
    pub fn new(uuid: &str) -> Result<Self, Error> {
        let nqn = make_nqn(uuid).into_cstring();
        let ss = NVMF_TGT
            .with(|t| {
                let tgt = t.borrow().tgt.as_ptr();
                unsafe {
                    spdk_nvmf_subsystem_create(
                        tgt,
                        nqn.as_ptr(),
                        SPDK_NVMF_SUBTYPE_NVME,
                        1,
                    )
                }
            })
            .to_result(|_| Error::Subsystem {
                source: Errno::EEXIST,
                nqn: uuid.into(),
                msg: "ss ptr is null".into(),
            })?;

        // Register subsystem event handler.
        unsafe {
            spdk_nvmf_subsystem_set_event_cb(
                ss.as_ptr(),
                Some(NvmfSubsystem::nvmf_subsystem_event_handler),
                std::ptr::null_mut(),
            )
        };

        // Use truncated SHA256 digest of Bdev UUID or name for subsystem
        // serial number.
        let sn = if let Some(nn) = Bdev::<()>::lookup_by_name(uuid) {
            make_sn(nn.uuid().as_bytes())
        } else {
            make_sn(uuid)
        };

        unsafe { spdk_nvmf_subsystem_set_sn(ss.as_ptr(), sn.as_ptr()) }
            .to_result(|e| Error::Subsystem {
                source: Errno::from_i32(e),
                nqn: uuid.into(),
                msg: "failed to set serial".into(),
            })?;

        let mn = CString::new(NVME_CONTROLLER_MODEL_ID).unwrap();
        unsafe { spdk_nvmf_subsystem_set_mn(ss.as_ptr(), mn.as_ptr()) }
            .to_result(|e| Error::Subsystem {
                source: Errno::from_i32(e),
                nqn: uuid.into(),
                msg: "failed to set model number".into(),
            })?;

        Ok(NvmfSubsystem(ss))
    }

    /// unfortunately, we cannot always use the bdev UUID which is a shame and
    /// mostly due to testing.
    pub fn new_with_uuid(
        uuid: &str,
        bdev: &UntypedBdev,
    ) -> Result<Self, Error> {
        let ss = NvmfSubsystem::new(uuid)?;
        ss.set_ana_reporting(false)?;
        ss.allow_any(false);
        ss.add_namespace(bdev, None)?;
        Ok(ss)
    }

    /// add the given bdev to this namespace
    pub fn add_namespace<T>(
        &self,
        bdev: &Bdev<T>,
        ptpl: Option<&std::path::PathBuf>,
    ) -> Result<(), Error>
    where
        T: spdk_rs::BdevOps,
    {
        let opts = struct_size_init!(
            spdk_nvmf_ns_opts {
                nsid: 0,
                nguid: *bdev.uuid().as_bytes(),
                eui64: unsafe { zeroed() },
                uuid: Default::default(),
                reserved44: unsafe { zeroed() },
                anagrpid: 0,
                no_auto_visible: false,
                reserved61: unsafe { zeroed() },
                transport_specific: ptr::null(),
            },
            opts_size
        );

        let bdev_cname = CString::new(bdev.name()).unwrap();
        let ptpl = ptpl.map(|ptpl| {
            CString::new(ptpl.to_string_lossy().to_string()).unwrap()
        });
        let ptpl_ptr = match &ptpl {
            Some(ptpl) => ptpl.as_ptr(),
            None => ptr::null_mut(),
        };
        let ns_id = unsafe {
            spdk_nvmf_subsystem_add_ns_ext(
                self.0.as_ptr(),
                bdev_cname.as_ptr(),
                &opts as *const _,
                opts.opts_size,
                ptpl_ptr,
            )
        };

        // the first namespace should be 1 and we do not (currently) use
        // more than one namespace

        if ns_id < 1 {
            Err(Error::Namespace {
                bdev: bdev.name().to_string(),
                msg: "failed to add namespace ID".to_string(),
            })
        } else {
            debug!(?bdev, ?ns_id, "added as namespace");
            Ok(())
        }
    }

    /// Removes the namespace and destroys the subsystem.
    ///
    /// # Safety
    ///
    /// The subsystem must paused or stopped.
    pub unsafe fn shutdown_unsafe(&self) -> i32 {
        if spdk_nvmf_subsystem_remove_ns(self.0.as_ptr(), 1) != 0 {
            error!(?self, "failed to remove namespace while destroying");
        }

        self.destroy_unsafe()
    }

    /// Destroys the SPDK object for subsystem.
    ///
    /// # Safety
    ///
    /// The subsystem must paused or stopped.
    unsafe fn destroy_unsafe(&self) -> i32 {
        if (*self.0.as_ptr()).destroying {
            warn!("Subsystem destruction already started");
            return -libc::EALREADY;
        }

        spdk_nvmf_subsystem_destroy(self.0.as_ptr(), None, std::ptr::null_mut())
    }

    /// Get NVMe subsystem's NQN
    pub fn get_nqn(&self) -> String {
        unsafe {
            spdk_nvmf_subsystem_get_nqn(self.0.as_ptr())
                .as_str()
                .to_string()
        }
    }

    fn cstr(host: &str) -> Result<CString, Error> {
        CString::new(host).map_err(|_| Error::HostCstrNul {
            host: host.to_string(),
        })
    }

    /// Allow any host to connect to the subsystem.
    pub fn allow_any(&self, enable: bool) {
        unsafe {
            spdk_nvmf_subsystem_set_allow_any_host(self.0.as_ptr(), enable);
        }
    }

    /// Get a list with all the host nqn's allowed to connect to this subsystem.
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts = Vec::with_capacity(4);

        let mut host =
            unsafe { spdk_nvmf_subsystem_get_first_host(self.0.as_ptr()) };

        while !host.is_null() {
            let host_str = unsafe { (*host).nqn.as_str() };

            hosts.push(host_str.to_string());

            host = unsafe {
                spdk_nvmf_subsystem_get_next_host(self.0.as_ptr(), host)
            };
        }

        hosts
    }

    /// Sets the allowed hosts to connect to the subsystem.
    /// It also disallows and disconnects any previously registered host.
    /// # Warning
    ///
    /// It does not disconnect non-registered hosts, eg: hosts which
    /// were connected before the allowed_hosts was configured.
    pub async fn set_allowed_hosts<H: AsRef<str>>(
        &self,
        hosts: &[H],
    ) -> Result<(), Error> {
        if hosts.is_empty() {
            return Ok(());
        }

        let hosts = hosts.iter().map(AsRef::as_ref).collect::<Vec<&str>>();
        self.allow_hosts(&hosts)?;

        let mut host =
            unsafe { spdk_nvmf_subsystem_get_first_host(self.0.as_ptr()) };

        let mut hosts_to_disconnect = vec![];
        {
            // must first "clone" the host's nqn as the disallow_host fn will
            // actually free the spdk_nvmf_host memory as it's not ref counted.
            // this also means we better not call any async code within this
            // "clone".
            while !host.is_null() {
                let host_str = unsafe { (*host).nqn.as_str() };
                if !hosts.contains(&host_str) {
                    hosts_to_disconnect.push(host_str.to_string());
                }
                host = unsafe {
                    spdk_nvmf_subsystem_get_next_host(self.0.as_ptr(), host)
                };
            }
        }

        for host in hosts_to_disconnect {
            self.disallow_host(&host)?;
            // note this only disconnects previously registered hosts
            // todo: disconnect any connected host which is not allowed
            self.disconnect_host(&host).await?;
        }

        Ok(())
    }

    /// Allows the specified hosts to connect to the subsystem.
    pub fn allow_hosts(&self, hosts: &[&str]) -> Result<(), Error> {
        for host in hosts {
            self.allow_host(host)?;
        }
        Ok(())
    }

    /// Allows a host to connect to the subsystem.
    pub fn allow_host(&self, host: &str) -> Result<(), Error> {
        let host = Self::cstr(host)?;
        unsafe {
            spdk_nvmf_subsystem_add_host(
                self.0.as_ptr(),
                host.as_ptr(),
                std::ptr::null_mut(),
            )
        }
        .to_result(|errno| Error::Subsystem {
            source: Errno::from_i32(errno),
            nqn: self.get_nqn(),
            msg: format!("failed to add allowed host: {host:?}"),
        })
    }

    /// Disallow hosts from connecting to the subsystem.
    pub fn disallow_hosts(&self, hosts: &[String]) -> Result<(), Error> {
        for host in hosts {
            self.disallow_host(host)?;
        }
        Ok(())
    }

    /// Disallow a host from connecting to the subsystem.
    pub fn disallow_host(&self, host: &str) -> Result<(), Error> {
        let host = Self::cstr(host)?;
        unsafe {
            spdk_nvmf_subsystem_remove_host(self.0.as_ptr(), host.as_ptr())
        }
        .to_result(|errno| Error::Subsystem {
            source: Errno::from_i32(errno),
            nqn: self.get_nqn(),
            msg: format!("failed to remove allowed host: {host:?}"),
        })?;
        Ok(())
    }

    /// Disconnect host from the subsystem.
    pub async fn disconnect_host(&self, host: &str) -> Result<(), Error> {
        extern "C" fn done_cb(arg: *mut c_void, status: i32) {
            let s = unsafe { Box::from_raw(arg as *mut oneshot::Sender<i32>) };
            s.send(status).ok();
        }

        let host_cstr = Self::cstr(host)?;
        let (s, r) = oneshot::channel::<i32>();
        unsafe {
            spdk_nvmf_subsystem_disconnect_host(
                self.0.as_ptr(),
                host_cstr.as_ptr(),
                Some(done_cb),
                cb_arg(s),
            );
        }

        r.await.expect("done_cb callback gone").to_result(|error| {
            Error::Subsystem {
                source: Errno::from_i32(error),
                msg: "Failed to disconnect host".to_string(),
                nqn: host.to_owned(),
            }
        })
    }

    /// enable Asymmetric Namespace Access (ANA) reporting
    pub fn set_ana_reporting(&self, enable: bool) -> Result<(), Error> {
        match std::env::var("NEXUS_NVMF_ANA_ENABLE") {
            Ok(s) => {
                if s != "1" {
                    return Ok(());
                }
            }
            Err(_) => {
                return Ok(());
            }
        }
        unsafe {
            spdk_nvmf_subsystem_set_ana_reporting(self.0.as_ptr(), enable)
        }
        .to_result(|e| Error::Subsystem {
            source: Errno::from_i32(e),
            nqn: self.get_nqn(),
            msg: format!("failed to set ANA reporting, enable {enable}"),
        })?;
        Ok(())
    }

    /// set controller ID range
    pub fn set_cntlid_range(
        &self,
        cntlid_min: u16,
        cntlid_max: u16,
    ) -> Result<(), Error> {
        unsafe {
            nvmf_subsystem_set_cntlid_range(
                self.0.as_ptr(),
                cntlid_min,
                cntlid_max,
            )
        }
        .to_result(|e| Error::Subsystem {
            source: Errno::from_i32(e),
            nqn: self.get_nqn(),
            msg: format!("failed to set controller ID range [{cntlid_min}, {cntlid_max}]"),
        })?;
        Ok(())
    }

    // we currently allow all listeners to the subsystem
    async fn add_listener(&self) -> Result<(), Error> {
        extern "C" fn listen_cb(arg: *mut c_void, status: i32) {
            let s = unsafe { Box::from_raw(arg as *mut oneshot::Sender<i32>) };
            s.send(status).unwrap();
        }

        let cfg = Config::get();

        // dont yet enable both ports, IOW just add one transportID now

        let trid_replica = TransportId::new(cfg.nexus_opts.nvmf_replica_port);

        let (s, r) = oneshot::channel::<i32>();
        unsafe {
            spdk_nvmf_subsystem_add_listener(
                self.0.as_ptr(),
                trid_replica.as_ptr(),
                Some(listen_cb),
                cb_arg(s),
            );
        }

        r.await.expect("listener callback gone").to_result(|e| {
            Error::Transport {
                source: Errno::from_i32(e),
                msg: "Failed to add listener".to_string(),
            }
        })
    }

    /// TODO
    async fn change_state(
        &self,
        op: &str,
        f: impl Fn(
            *mut spdk_nvmf_subsystem,
            spdk_nvmf_subsystem_state_change_done,
            *mut c_void,
        ) -> i32,
    ) -> Result<(), Error> {
        extern "C" fn state_change_cb(
            _ss: *mut spdk_nvmf_subsystem,
            arg: *mut c_void,
            status: i32,
        ) {
            let s = unsafe { Box::from_raw(arg as *mut oneshot::Sender<i32>) };
            s.send(status).unwrap();
        }

        info!(?self, "Subsystem {} in progress...", op);

        let res = {
            let mut n = 0;

            let (rc, r) = loop {
                let (s, r) = oneshot::channel::<i32>();

                let rc = -f(self.0.as_ptr(), Some(state_change_cb), cb_arg(s));

                if rc != libc::EBUSY || n >= 3 {
                    break (rc, r);
                }

                n += 1;

                warn!(
                    "Failed to {} '{}': subsystem is busy, retrying {}...",
                    op,
                    self.get_nqn(),
                    n
                );

                crate::sleep::mayastor_sleep(std::time::Duration::from_millis(
                    100,
                ))
                .await
                .unwrap();
            };

            match rc {
                0 => r.await.unwrap().to_result(|e| Error::Subsystem {
                    source: Errno::from_i32(e),
                    nqn: self.get_nqn(),
                    msg: format!("{op} failed"),
                }),
                libc::EBUSY => Err(Error::SubsystemBusy {
                    nqn: self.get_nqn(),
                    op: op.to_owned(),
                }),
                e => Err(Error::Subsystem {
                    source: Errno::from_i32(e),
                    nqn: self.get_nqn(),
                    msg: format!("failed to initiate {op}"),
                }),
            }
        };

        if let Err(ref e) = res {
            error!(?self, "Subsystem {} failed: {}", op, e.to_string());
        } else {
            info!(?self, "Subsystem {} completed: Ok", op);
        }

        res
    }

    /// start the subsystem previously created -- note that we destroy it on
    /// failure to ensure the state is not in limbo and to avoid leaking
    /// resources
    pub async fn start(self) -> Result<String, Error> {
        self.add_listener().await?;

        if let Err(e) = self
            .change_state("start", |ss, cb, arg| unsafe {
                spdk_nvmf_subsystem_start(ss, cb, arg)
            })
            .await
        {
            error!(
                "Failed to start subsystem '{}': {}; destroying it",
                self.get_nqn(),
                e.to_string(),
            );

            unsafe {
                self.shutdown_unsafe();
            }

            Err(e)
        } else {
            Ok(self.get_nqn())
        }
    }

    /// stop the subsystem
    pub async fn stop(&self) -> Result<(), Error> {
        self.change_state("stop", |ss, cb, arg| unsafe {
            spdk_nvmf_subsystem_stop(ss, cb, arg)
        })
        .await
    }

    /// transition the subsystem to paused state
    /// intended to be a temporary state while changes are made
    pub async fn pause(&self) -> Result<(), Error> {
        self.change_state("pause", |ss, cb, arg| unsafe {
            spdk_nvmf_subsystem_pause(ss, 1, cb, arg)
        })
        .await
    }

    /// transition the subsystem to active state
    pub async fn resume(&self) -> Result<(), Error> {
        self.change_state("resume", |ss, cb, arg| unsafe {
            spdk_nvmf_subsystem_resume(ss, cb, arg)
        })
        .await
    }

    /// get ANA state
    pub async fn get_ana_state(&self) -> Result<u32, Error> {
        let cfg = Config::get();
        let trid_replica = TransportId::new(cfg.nexus_opts.nvmf_replica_port);
        let listener = unsafe {
            nvmf_subsystem_find_listener(self.0.as_ptr(), trid_replica.as_ptr())
        };
        if listener.is_null() {
            Err(Error::Listener {
                nqn: self.get_nqn(),
                trid: trid_replica.to_string(),
            })
        } else {
            Ok(unsafe { *(*listener).ana_state })
        }
    }

    /// set ANA state: optimized, non_optimized, inaccessible
    /// subsystem must be in paused or inactive state
    pub async fn set_ana_state(&self, ana_state: u32) -> Result<(), Error> {
        extern "C" fn set_ana_state_cb(arg: *mut c_void, status: i32) {
            let s = unsafe { Box::from_raw(arg as *mut oneshot::Sender<i32>) };
            s.send(status).unwrap();
        }
        let cfg = Config::get();
        let trid_replica = TransportId::new(cfg.nexus_opts.nvmf_replica_port);

        let (s, r) = oneshot::channel::<i32>();

        unsafe {
            spdk_nvmf_subsystem_set_ana_state(
                self.0.as_ptr(),
                trid_replica.as_ptr(),
                ana_state,
                0,
                Some(set_ana_state_cb),
                cb_arg(s),
            );
        }

        r.await
            .expect("Cancellation is not supported")
            .to_result(|e| Error::Subsystem {
                source: Errno::from_i32(-e),
                nqn: self.get_nqn(),
                msg: "failed to set_ana_state of the subsystem".to_string(),
            })
    }

    /// destroy all subsystems associated with our target, subsystems must be in
    /// stopped state
    pub fn destroy_all() {
        Reactors::master().send_future(async {
            // NvmfSubsystem::first().iter().for_each(|s| s.destroy());
            NVMF_TGT.with(|t| {
                let mut tgt = t.borrow_mut();
                tgt.next_state()
            })
        });
    }

    /// stop all subsystems
    pub async fn stop_all(tgt: *mut spdk_nvmf_tgt) {
        let subsystem = unsafe {
            NonNull::new(spdk_nvmf_subsystem_get_first(tgt)).map(NvmfSubsystem)
        };

        if let Some(subsystem) = subsystem {
            for s in subsystem.into_iter() {
                if let Err(e) = s.stop().await {
                    error!(
                        "Failed to stop subsystem '{}': {}",
                        s.get_nqn(),
                        e.to_string()
                    );
                }
            }
        }
    }

    /// Get the first subsystem within the system
    pub fn first() -> Option<NvmfSubsystem> {
        NVMF_TGT.with(|t| {
            let ss = unsafe {
                spdk_nvmf_subsystem_get_first(t.borrow().tgt.as_ptr())
            };

            if ss.is_null() {
                None
            } else {
                Some(NvmfSubsystem(NonNull::new(ss).unwrap()))
            }
        })
    }

    /// lookup a subsystem by its UUID
    pub fn nqn_lookup(uuid: &str) -> Option<NvmfSubsystem> {
        let nqn = make_nqn(uuid);
        NvmfSubsystem::first()
            .unwrap()
            .into_iter()
            .find(|s| s.get_nqn() == nqn)
    }

    /// get the bdev associated with this subsystem -- we implicitly assume the
    /// first namespace
    pub fn bdev(&self) -> Option<UntypedBdev> {
        let ns = unsafe { spdk_nvmf_subsystem_get_first_ns(self.0.as_ptr()) };

        if ns.is_null() {
            return None;
        }

        Bdev::checked_from_ptr(unsafe { spdk_nvmf_ns_get_bdev(ns) })
    }

    fn listeners_to_vec(&self) -> Option<Vec<TransportId>> {
        unsafe {
            let mut listener =
                spdk_nvmf_subsystem_get_first_listener(self.0.as_ptr());

            if listener.is_null() {
                return None;
            }

            let mut ids = vec![TransportId(
                *spdk_nvmf_subsystem_listener_get_trid(listener),
            )];

            loop {
                listener = spdk_nvmf_subsystem_get_next_listener(
                    self.0.as_ptr(),
                    listener,
                );
                if !listener.is_null() {
                    ids.push(TransportId(
                        *spdk_nvmf_subsystem_listener_get_trid(listener),
                    ));
                    continue;
                } else {
                    break;
                }
            }
            Some(ids)
        }
    }

    pub fn subtype(&self) -> SubType {
        unsafe {
            match self.0.as_ref().subtype {
                SPDK_NVMF_SUBTYPE_DISCOVERY => SubType::Discovery,
                SPDK_NVMF_SUBTYPE_NVME => SubType::Nvme,
                _ => panic!("unknown NVMe subtype"),
            }
        }
    }

    /// return the URI's this subsystem is listening on
    pub fn uri_endpoints(&self) -> Option<Vec<String>> {
        if let Some(v) = self.listeners_to_vec() {
            let nqn = self.get_nqn();
            Some(v.iter().map(|t| format!("{t}/{nqn}")).collect::<Vec<_>>())
        } else {
            None
        }
    }
}

/// Makes an NQN froma UUID.
fn make_nqn(id: &str) -> String {
    format!("{NVME_NQN_PREFIX}:{id}")
}

/// NQN target.
pub enum NqnTarget<'a> {
    Nexus(&'a Nexus<'a>),
    Replica(Lvol),
    None,
}

impl<'a> NqnTarget<'a> {
    pub fn lookup(nqn: &str) -> Self {
        let Some(bdev) = UntypedBdev::bdev_first() else {
            return Self::None;
        };

        let parts: Vec<&str> = nqn.split(':').collect();
        if parts.len() != 2 || parts[0] != NVME_NQN_PREFIX {
            return Self::None;
        }

        let name = parts[1];

        for b in bdev.into_iter() {
            match b.driver() {
                NEXUS_MODULE_NAME if b.name() == name => {
                    return Self::Nexus(unsafe {
                        Nexus::unsafe_from_untyped_bdev(*b)
                    });
                }
                "lvol" if b.name() == name => {
                    return Lvol::try_from(b).map_or(Self::None, Self::Replica)
                }
                _ => {}
            }
        }

        Self::None
    }
}
