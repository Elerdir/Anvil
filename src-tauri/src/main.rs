// Bez tohohle by se na Windows v release buildu vedle okna otevřela i konzole.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    anvil_lib::run()
}
