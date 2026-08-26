//! Eggs bundled with the panel. Seeded into the database on startup
//! (`INSERT OR IGNORE`, so admin-imported versions always win).

use crate::db::Db;

pub const BUNDLED: &[&str] = &[
    include_str!("../../../eggs/vanilla-minecraft.json"),
    include_str!("../../../eggs/paper.json"),
    include_str!("../../../eggs/fabric.json"),
    include_str!("../../../eggs/forge.json"),
    include_str!("../../../eggs/velocity.json"),
    include_str!("../../../eggs/bungeecord.json"),
    include_str!("../../../eggs/mindustry.json"),
    include_str!("../../../eggs/factorio.json"),
    include_str!("../../../eggs/teamspeak3.json"),
    include_str!("../../../eggs/openttd.json"),
    include_str!("../../../eggs/pocketmine.json"),
];

/// Insert every bundled egg that isn't present yet. Returns how many were added.
pub fn seed(db: &Db) -> anyhow::Result<usize> {
    let mut added = 0;
    for raw in BUNDLED {
        let egg = nucleus_core::import_ptero_egg(raw)?;
        let inserted = db.with(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO eggs (slug, name, json, created_at) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    egg.slug,
                    egg.name,
                    serde_json::to_string(&egg)?,
                    chrono::Utc::now().timestamp()
                ],
            )?;
            Ok(n)
        })?;
        if inserted > 0 {
            added += 1;
            tracing::info!(slug = %egg.slug, "bundled egg seeded");
        }
    }
    Ok(added)
}
