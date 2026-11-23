//! Verified offline-safe model registry and local import verification (§7.3, §11.3, REQ-021).
//!
//! Strict supply-chain invariants:
//! - Models are separate artifacts from code, with distinct `code_license` and `weights_license`.
//! - ZERO implicit downloads; ZERO execution of pickles/arbitrary scripts.
//! - Local user-provided model files are verified against SHA-256 and exact byte size before activation.
//! - When an optional neural model is absent, returns explicit `ModelMissing` without faking ML.

use karaok_media::hash_file_sha256;
use karaok_types::{
    ErrorCode, KaraokError, KaraokResult, ModelCapability, ModelInstallState, ModelManifest,
};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn is_verified_license_string(lic: &str) -> bool {
    let lower = lic.trim().to_lowercase();
    !lower.is_empty()
        && !lower.contains("unknown")
        && !lower.contains("à vérifier")
        && !lower.contains("revue requise")
}

/// Local verified model store rooted in macOS Application Support (never in auto-purged Caches §10).
#[derive(Debug, Clone)]
pub struct ModelStore {
    models_dir: PathBuf,
    manifests: Vec<ModelManifest>,
}

impl ModelStore {
    pub fn open(models_dir: PathBuf) -> KaraokResult<Self> {
        fs::create_dir_all(&models_dir).map_err(|e| {
            KaraokError::new(
                ErrorCode::IoError,
                format!("Impossible d'initialiser le dossier modèles : {e}"),
            )
        })?;
        let mut store = Self {
            models_dir,
            manifests: Self::default_catalog(),
        };
        store.refresh_installed_states();
        Ok(store)
    }

    /// Reference catalog of optional neural models that require explicit user opt-in & weight verification.
    /// No weights are bundled or silently downloaded.
    fn default_catalog() -> Vec<ModelManifest> {
        vec![
            ModelManifest {
                id: "htdemucs-onnx-opt-in".to_string(),
                display_name: "Séparation neuronale ONNX (import local vérifié)".to_string(),
                capability: ModelCapability::StemSeparation,
                version: "pending-verified-checkpoint".to_string(),
                official_source_url: "https://github.com/facebookresearch/demucs".to_string(),
                sha256: "UNCONFIGURED_REQUIRES_VERIFIED_MANIFEST".to_string(),
                size_bytes: 0,
                author: "Meta Research / communauté Demucs (dépôt archivé — audit requis)".to_string(),
                code_license: "MIT".to_string(),
                weights_license: "À vérifier par checkpoint exact avant adoption".to_string(),
                languages: vec!["multi".to_string()],
                runtime: "onnx-cpu-or-coreml".to_string(),
                verified_date: "2026-11-23".to_string(),
                local_path: None,
                state: ModelInstallState::NotInstalled,
            },
            ModelManifest {
                id: "sofa-singing-aligner-opt-in".to_string(),
                display_name: "Aligneur de chant SOFA (import local vérifié)".to_string(),
                capability: ModelCapability::LyricsAlignment,
                version: "pending-verified-checkpoint".to_string(),
                official_source_url: "https://github.com/qiuqiao/SOFA".to_string(),
                sha256: "UNCONFIGURED_REQUIRES_VERIFIED_MANIFEST".to_string(),
                size_bytes: 0,
                author: "SOFA Contributors".to_string(),
                code_license: "MIT".to_string(),
                weights_license: "Dépend du checkpoint de langue choisi — revue requise".to_string(),
                languages: vec!["en".to_string(), "fr".to_string(), "ja".to_string()],
                runtime: "onnx".to_string(),
                verified_date: "2026-11-23".to_string(),
                local_path: None,
                state: ModelInstallState::NotInstalled,
            },
        ]
    }

    pub fn list_manifests(&self) -> &[ModelManifest] {
        &self.manifests
    }

    pub fn refresh_installed_states(&mut self) {
        let manifest_file = self.models_dir.join("installed_manifests.json");
        if let Ok(raw) = fs::read_to_string(&manifest_file) {
            if let Ok(saved) = serde_json::from_str::<Vec<ModelManifest>>(&raw) {
                for item in saved {
                    if let Some(existing) = self.manifests.iter_mut().find(|m| m.id == item.id) {
                        *existing = item;
                    } else {
                        self.manifests.push(item);
                    }
                }
            }
        }
        // Re-verify any claimed installed local model against its recorded SHA-256!
        for manifest in &mut self.manifests {
            if let Some(ref path_str) = manifest.local_path {
                let p = Path::new(path_str);
                if !p.exists() {
                    manifest.state = ModelInstallState::NotInstalled;
                    continue;
                }
                if !is_verified_license_string(&manifest.code_license)
                    || !is_verified_license_string(&manifest.weights_license)
                {
                    manifest.state = ModelInstallState::LicenseUnverified;
                    continue;
                }
                match hash_file_sha256(p) {
                    Ok((actual_hash, actual_size))
                        if actual_hash.eq_ignore_ascii_case(&manifest.sha256)
                            && actual_size == manifest.size_bytes =>
                    {
                        manifest.state = ModelInstallState::InstalledVerified;
                    }
                    _ => {
                        manifest.state = ModelInstallState::HashMismatch;
                    }
                }
            }
        }
    }

    /// Import a local `.onnx` model file with strict SHA-256, size, extension, and license verification (§11.3).
    /// Rejects pickle (`.pkl`, `.pt`, `.pth`, `.ckpt`), scripts, missing licenses, and hash mismatches!
    pub fn import_verified_local_model(
        &mut self,
        source_file: &Path,
        mut manifest: ModelManifest,
    ) -> KaraokResult<ModelManifest> {
        let ext = source_file
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if ext != "onnx" {
            return Err(KaraokError::with_hint(
                ErrorCode::ModelUnverified,
                format!("Format de poids « .{ext} » refusé : seuls les artefacts ONNX statiques (.onnx) sont autorisés (aucun pickle ni script exécutable)."),
                "Convertissez ou fournissez un modèle au format .onnx accompagné de son empreinte SHA-256 et de sa licence poids.",
            ));
        }
        if !is_verified_license_string(&manifest.code_license)
            || !is_verified_license_string(&manifest.weights_license)
        {
            return Err(KaraokError::new(
                ErrorCode::ModelUnverified,
                "Licence du code et licence des poids vérifiées obligatoires avant adoption d'un modèle.",
            ));
        }
        if manifest.sha256.len() != 64 || !manifest.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(KaraokError::new(
                ErrorCode::ModelUnverified,
                "Empreinte SHA-256 attendue invalide (64 caractères hexadécimaux requis).",
            ));
        }

        let safe_id: String = manifest
            .id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if safe_id.is_empty() {
            return Err(KaraokError::new(
                ErrorCode::InvalidParameter,
                "Identifiant de modèle invalide.",
            ));
        }

        // Copy to unique temp file FIRST, then verify SHA-256 & size on the copied temp file (eliminates TOCTOU - I9!)
        let tmp = self.models_dir.join(format!(".tmp-{safe_id}-{}.part", Uuid::new_v4()));
        fs::copy(source_file, &tmp).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            KaraokError::new(ErrorCode::IoError, format!("Copie du modèle impossible : {e}"))
        })?;

        let (actual_hash, actual_size) = match hash_file_sha256(&tmp) {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
        };
        if !actual_hash.eq_ignore_ascii_case(&manifest.sha256) {
            let _ = fs::remove_file(&tmp);
            return Err(KaraokError::new(
                ErrorCode::ModelUnverified,
                format!(
                    "Vérification SHA-256 échouée (attendu {}, observé {}). Modèle refusé.",
                    &manifest.sha256[..12],
                    &actual_hash[..12]
                ),
            ));
        }
        if manifest.size_bytes > 0 && actual_size != manifest.size_bytes {
            let _ = fs::remove_file(&tmp);
            return Err(KaraokError::new(
                ErrorCode::ModelUnverified,
                "La taille du fichier modèle ne correspond pas au manifeste.",
            ));
        }

        let dest = self.models_dir.join(format!("{safe_id}-{actual_hash}.onnx"));
        fs::rename(&tmp, &dest).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            KaraokError::new(
                ErrorCode::IoError,
                format!("Installation atomique du modèle échouée : {e}"),
            )
        })?;

        manifest.size_bytes = actual_size;
        manifest.local_path = Some(dest.to_string_lossy().to_string());
        manifest.state = ModelInstallState::InstalledVerified;

        if let Some(existing) = self.manifests.iter_mut().find(|m| m.id == manifest.id) {
            *existing = manifest.clone();
        } else {
            self.manifests.push(manifest.clone());
        }
        self.persist_manifests()?;
        Ok(manifest)
    }

    pub fn verified_model_for(&self, capability: ModelCapability) -> Option<&ModelManifest> {
        self.manifests
            .iter()
            .find(|m| m.capability == capability && m.state == ModelInstallState::InstalledVerified)
    }

    fn persist_manifests(&self) -> KaraokResult<()> {
        let target = self.models_dir.join("installed_manifests.json");
        let tmp = self
            .models_dir
            .join(format!(".tmp-manifests-{}.json", Uuid::new_v4()));
        let json = serde_json::to_string_pretty(&self.manifests).map_err(|e| {
            KaraokError::new(ErrorCode::InternalError, format!("Sérialisation manifestes : {e}"))
        })?;
        fs::write(&tmp, json).map_err(|e| {
            KaraokError::new(ErrorCode::IoError, format!("Écriture manifestes : {e}"))
        })?;
        fs::rename(&tmp, &target).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            KaraokError::new(ErrorCode::IoError, format!("Publication manifestes : {e}"))
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_pickle_and_hash_mismatch_and_accepts_verified_onnx() {
        let dir = tempdir().unwrap();
        let mut store = ModelStore::open(dir.path().join("models")).unwrap();

        let pkl = dir.path().join("evil.pkl");
        fs::write(&pkl, b"pickle").unwrap();
        let mut m = store.list_manifests()[0].clone();
        m.weights_license = "MIT".into();
        assert_eq!(
            store.import_verified_local_model(&pkl, m.clone()).unwrap_err().code,
            ErrorCode::ModelUnverified
        );

        let onnx = dir.path().join("sep.onnx");
        fs::write(&onnx, b"ONNX-TEST-WEIGHTS").unwrap();
        let (real_hash, real_size) = hash_file_sha256(&onnx).unwrap();

        m.sha256 = "0".repeat(64);
        assert_eq!(
            store.import_verified_local_model(&onnx, m.clone()).unwrap_err().code,
            ErrorCode::ModelUnverified
        );

        m.sha256 = real_hash;
        m.size_bytes = real_size;
        let installed = store.import_verified_local_model(&onnx, m).unwrap();
        assert_eq!(installed.state, ModelInstallState::InstalledVerified);
        assert!(store.verified_model_for(ModelCapability::StemSeparation).is_some());
    }
}
