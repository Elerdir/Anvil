//! Na čem se počítá: **které grafické zařízení** a **kolik vláken**.
//!
//! Obojí se ptá systému, ne modelu, a obojí má jednu společnou past — sáhnout
//! po tom, co je po ruce, je špatně:
//!
//! * llama.cpp bez explicitní volby vezme nulté zařízení, které mu ohlásí
//!   driver. Na notebooku s hybridní grafikou to bývá **integrovaná** karta,
//!   která jede ze stejné systémové paměti jako experti — nepřidá propustnost
//!   a přidá kopírování. Ještě horší je výchozí `split_mode = Layer`: ten
//!   rozdělí vrstvy mezi *všechna* zařízení, takže část modelu skončí na iGPU
//!   i tehdy, když si dedikovanou kartu vybereme. Proto se zařízení předává
//!   přes `with_devices(&[index])` — jedno zařízení, žádné dělení.
//! * Počet vláken se nebere z logických procesorů, ale z **fyzických jader**.
//!   SMT sourozenci u decode nepřidají propustnost (ta je omezená pamětí, ne
//!   výpočtem) a jen přidají synchronizaci.
//!
//! Seznam zařízení dává ggml (`list_llama_ggml_backend_devices`), takže indexy
//! sedí přesně s tím, co `with_devices` očekává, a rovnou nesou i typ zařízení
//! a velikost paměti. **Volat až po `LlamaBackend::init()`** — dřív ggml žádné
//! backendy zaregistrované nemá a seznam vyjde prázdný.

use llama_cpp_2::{list_llama_ggml_backend_devices, LlamaBackendDeviceType};

use super::offload_plan::MachineProfile;

/// Druh zařízení, jak ho hlásí ggml.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Dedikovaná karta s vlastní pamětí.
    Discrete,
    /// Integrovaná grafika — sdílí systémovou RAM.
    Integrated,
    Cpu,
    Other,
}

/// Grafické (nebo jiné výpočetní) zařízení nabídnuté ggml.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeDevice {
    /// Index pro `LlamaModelParams::with_devices`.
    pub index: usize,
    /// Popis pro člověka, např. „NVIDIA GeForce RTX 4070 Laptop GPU".
    pub description: String,
    /// Backend, který zařízení nabídl („Vulkan", „Metal", „CPU").
    pub backend: String,
    pub kind: DeviceKind,
    pub memory_total: u64,
    pub memory_free: u64,
}

impl ComputeDevice {
    /// Zařízení, na kterém má smysl počítat (tj. ne CPU).
    pub fn is_gpu(&self) -> bool {
        matches!(self.kind, DeviceKind::Discrete | DeviceKind::Integrated)
    }
}

/// Zařízení, která ggml zná. Prázdný seznam = backend ještě není
/// inicializovaný, nebo build nemá žádný GPU backend.
pub fn list_devices() -> Vec<ComputeDevice> {
    list_llama_ggml_backend_devices()
        .into_iter()
        .map(|d| ComputeDevice {
            index: d.index,
            description: if d.description.is_empty() {
                d.name.clone()
            } else {
                d.description.clone()
            },
            backend: d.backend,
            kind: match d.device_type {
                LlamaBackendDeviceType::Gpu => DeviceKind::Discrete,
                LlamaBackendDeviceType::IntegratedGpu => DeviceKind::Integrated,
                LlamaBackendDeviceType::Cpu => DeviceKind::Cpu,
                _ => DeviceKind::Other,
            },
            memory_total: d.memory_total as u64,
            memory_free: d.memory_free as u64,
        })
        .collect()
}

/// Které zařízení použít. Dedikovaná karta vyhrává nad integrovanou vždycky;
/// mezi kartami téhož druhu rozhoduje větší paměť a pak nižší index (pořadí
/// od ggml pochází od driveru a první bývá primární karta).
pub fn choose_device(devices: &[ComputeDevice]) -> Option<&ComputeDevice> {
    devices.iter().filter(|d| d.is_gpu()).min_by_key(|d| {
        (
            match d.kind {
                DeviceKind::Discrete => 0,
                _ => 1,
            },
            std::cmp::Reverse(d.memory_total),
            d.index,
        )
    })
}

/// Sdílí CPU a GPU jednu fyzickou paměť?
///
/// Tvrdíme to jen o Apple Silicon, a tam vždycky. x86 s integrovanou
/// grafikou paměť sice technicky sdílí taky, ale ovladač z ní GPU vyhradí
/// pevný výřez a přístup do zbytku RAM jde přes sběrnici — hybridní plán
/// (experti v RAM) tam proto dává smysl dál. Na Apple Silicon by naopak
/// znamenal jen přechod mezi backendy na každý token bez jakéhokoli zisku.
pub const fn has_unified_memory() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

/// Sestaví profil stroje pro `offload_plan::plan_offload`.
///
/// Vybere zařízení, přečte jeho **volnou** paměť (ne celkovou — na kartě,
/// která kreslí plochu, je rozdíl klidně přes gigabajt) a doplní počet
/// fyzických jader. Když žádná GPU není, vrátí CPU profil.
pub fn machine_profile() -> MachineProfile {
    let devices = list_devices();
    let chosen = choose_device(&devices);

    match chosen {
        Some(d) => {
            tracing::info!(
                device = %d.description,
                backend = %d.backend,
                free_mb = d.memory_free / (1024 * 1024),
                total_mb = d.memory_total / (1024 * 1024),
                unified = has_unified_memory(),
                "Zvolené výpočetní zařízení"
            );
            MachineProfile {
                vram_bytes: Some(d.memory_free),
                device_index: Some(d.index),
                cpu_cores: physical_cores(),
                unified_memory: has_unified_memory(),
            }
        }
        None => {
            tracing::info!("Žádné GPU zařízení — poběží se na CPU");
            MachineProfile::cpu_only(physical_cores())
        }
    }
}

/// Počet fyzických jader. Když ho nejde zjistit, vrátí počet logických
/// procesorů — na SMT stroji tím vyjde vláken víc, než je jader, což je podle
/// měření bez znatelného dopadu.
pub fn physical_cores() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    detect_physical_cores()
        .filter(|cores| *cores > 0)
        .map(|cores| cores.min(logical))
        .unwrap_or(logical)
}

#[cfg(windows)]
fn detect_physical_cores() -> Option<usize> {
    windows_cores::count()
}

#[cfg(not(windows))]
fn detect_physical_cores() -> Option<usize> {
    // Na Linuxu/macOS necháváme fallback na logické procesory — hodnota se
    // stejně jen mírně liší a nepotřebujeme kvůli ní další závislost.
    None
}

/// `GetLogicalProcessorInformationEx` napřímo. Přes WMI (`Win32_Processor`)
/// by se to ptalo stovky milisekund, a to při každém načtení modelu.
#[cfg(windows)]
mod windows_cores {
    const RELATION_PROCESSOR_CORE: u32 = 0;
    const ERROR_INSUFFICIENT_BUFFER: u32 = 122;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalProcessorInformationEx(
            relationship: u32,
            buffer: *mut u8,
            returned_length: *mut u32,
        ) -> i32;
        fn GetLastError() -> u32;
    }

    /// Počet záznamů typu `RelationProcessorCore` = počet fyzických jader.
    pub fn count() -> Option<usize> {
        let mut length: u32 = 0;

        // První volání musí selhat s ERROR_INSUFFICIENT_BUFFER a vrátit
        // potřebnou velikost.
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                std::ptr::null_mut(),
                &mut length,
            )
        };
        if ok != 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || length == 0 {
            return None;
        }

        let mut buffer = vec![0u8; length as usize];
        let ok = unsafe {
            GetLogicalProcessorInformationEx(
                RELATION_PROCESSOR_CORE,
                buffer.as_mut_ptr(),
                &mut length,
            )
        };
        if ok == 0 {
            return None;
        }

        Some(count_entries(&buffer[..length as usize]))
    }

    /// Struktury jsou proměnné délky — každá začíná dvojicí u32
    /// (Relationship, Size) a `Size` vede na další.
    fn count_entries(buffer: &[u8]) -> usize {
        let mut offset = 0usize;
        let mut cores = 0usize;

        while offset + 8 <= buffer.len() {
            let size = u32::from_le_bytes([
                buffer[offset + 4],
                buffer[offset + 5],
                buffer[offset + 6],
                buffer[offset + 7],
            ]) as usize;

            // Nulová nebo přetékající velikost by znamenala nekonečnou smyčku.
            if size == 0 || offset + size > buffer.len() {
                break;
            }

            cores += 1;
            offset += size;
        }

        cores
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn entry(size: u32) -> Vec<u8> {
            let mut v = Vec::new();
            v.extend_from_slice(&RELATION_PROCESSOR_CORE.to_le_bytes());
            v.extend_from_slice(&size.to_le_bytes());
            v.resize(size as usize, 0);
            v
        }

        #[test]
        fn counts_variable_length_entries() {
            let mut buffer = entry(48);
            buffer.extend(entry(64));
            buffer.extend(entry(48));
            assert_eq!(count_entries(&buffer), 3);
        }

        #[test]
        fn zero_size_does_not_loop_forever() {
            let mut buffer = entry(48);
            buffer.extend(entry(0));
            assert_eq!(count_entries(&buffer), 1);
        }

        #[test]
        fn truncated_tail_is_ignored() {
            let mut buffer = entry(48);
            buffer.extend_from_slice(&[0u8; 4]);
            assert_eq!(count_entries(&buffer), 1);
        }

        #[test]
        fn detects_something_on_this_machine() {
            let cores = count().expect("Windows musí topologii umět vrátit");
            assert!(cores > 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(index: usize, kind: DeviceKind, total_gb: u64) -> ComputeDevice {
        ComputeDevice {
            index,
            description: format!("dev{index}"),
            backend: "Vulkan".into(),
            kind,
            memory_total: total_gb * 1024 * 1024 * 1024,
            memory_free: total_gb * 1024 * 1024 * 1024,
        }
    }

    #[test]
    fn discrete_beats_integrated_even_when_listed_later() {
        // Přesně stav na notebooku s hybridní grafikou: iGPU je nultá.
        let devices = vec![
            device(0, DeviceKind::Integrated, 16),
            device(1, DeviceKind::Discrete, 8),
        ];
        assert_eq!(choose_device(&devices).unwrap().index, 1);
    }

    #[test]
    fn between_discrete_cards_more_memory_wins() {
        let devices = vec![
            device(0, DeviceKind::Discrete, 8),
            device(1, DeviceKind::Discrete, 24),
        ];
        assert_eq!(choose_device(&devices).unwrap().index, 1);
    }

    #[test]
    fn same_memory_falls_back_to_lower_index() {
        let devices = vec![
            device(2, DeviceKind::Discrete, 8),
            device(1, DeviceKind::Discrete, 8),
        ];
        assert_eq!(choose_device(&devices).unwrap().index, 1);
    }

    #[test]
    fn integrated_is_used_when_it_is_all_there_is() {
        let devices = vec![device(0, DeviceKind::Integrated, 16)];
        assert_eq!(choose_device(&devices).unwrap().index, 0);
    }

    #[test]
    fn cpu_device_is_never_chosen() {
        let devices = vec![device(0, DeviceKind::Cpu, 64)];
        assert!(choose_device(&devices).is_none());
    }

    #[test]
    fn physical_cores_are_sane() {
        let cores = physical_cores();
        assert!(cores >= 1);
        assert!(cores <= std::thread::available_parallelism().unwrap().get());
    }
}
