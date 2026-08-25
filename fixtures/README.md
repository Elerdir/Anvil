# Fixtures

Vstupy pro ověřování proti skutečnému modelu. **Nejsou součástí workspace** —
cargo je nestaví ani netestuje.

## vadny-projekt

Malý projekt se **záměrně zasazenými chybami**. Slouží k jedinému: odlišit
„projekt je čistý" od „model chyby nenajde". Nad skutečným kódem to rozlišit
nejde — tři běhy review nad `anvil-domain` skončily prakticky bez nálezu
a z toho neplyne vůbec nic.

**Kód v téhle složce je rozbitý schválně. Neopravovat.**

Klíč k odpovědím je `vadny-projekt-nalezy.json` **vedle** složky, ne v ní.
Uvnitř by si ho model přečetl přes `read_file` a měřila by se jeho schopnost
číst zadání.

```
scripts\review.bat D:\models\model.gguf fixtures\vadny-projekt
```
