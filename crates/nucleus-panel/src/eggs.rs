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
    include_str!("../../../eggs/cs2.json"),
    include_str!("../../../eggs/rust.json"),
    include_str!("../../../eggs/valheim.json"),
];

/// Insert every bundled egg that isn't present yet. Returns how many were added.
pub fn seed(db: &Db) -> anyhow::Result<usize> {
    let mut added = 0;
    for raw in BUNDLED {
        let egg = nucleus_core::import_ptero_egg(raw)?;
        let json = serde_json::to_string(&egg)?;
        let res = db.with(|c| {
            let existing: Option<String> = c
                .query_row("SELECT json FROM eggs WHERE slug = ?1", [&egg.slug], |r| {
                    r.get(0)
                })
                .ok();
            match existing {
                Some(old) => {
                    // Heal bundled stubs that shipped without an install
                    // script, and re-sync bundled eggs whose stored copy
                    // drifted from the bundle (e.g. image-order fixes).
                    let Ok(old_egg) = serde_json::from_str::<nucleus_core::Egg>(&old) else {
                        return Ok(0);
                    };
                    let stale = (old_egg.install_script.is_none()
                        && egg.install_script.is_some())
                        || (old_egg.install_script == egg.install_script
                            && old_egg.docker_images != egg.docker_images);
                    if stale {
                        c.execute(
                            "UPDATE eggs SET name = ?2, json = ?3 WHERE slug = ?1",
                            rusqlite::params![egg.slug, egg.name, json],
                        )?;
                        tracing::info!(slug = %egg.slug, "bundled egg upgraded (install script added)");
                    }
                    Ok(0)
                }
                None => {
                    c.execute(
                        "INSERT INTO eggs (slug, name, json, created_at) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![
                            egg.slug,
                            egg.name,
                            json,
                            chrono::Utc::now().timestamp()
                        ],
                    )?;
                    tracing::info!(slug = %egg.slug, "bundled egg seeded");
                    Ok(1)
                }
            }
        })?;
        added += res;
    }
    Ok(added)
}
