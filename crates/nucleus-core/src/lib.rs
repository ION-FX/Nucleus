pub mod curseforge;
pub mod dto;
pub mod egg;

pub use curseforge::{detect_pack_kind, CurseForgeManifest, ModrinthIndex, PackKind};
pub use dto::{
    CreateServerRequest, FileEntry, InstallStatus, Limits, PortMapping, PowerAction, PowerRequest,
    ServerStatus,
};
pub use egg::{import_ptero_egg, render_startup, Egg, EggVariable};

/// Generate a short server identifier used as container/dir name (`nucleus-<id>`).
pub fn new_server_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..10].to_string()
}
