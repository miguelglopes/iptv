use std::collections::HashMap;

use once_cell::sync::Lazy;

use crate::canonical::canonical_key;

const ORDER: &[&str] = &[
    "RTP 1", "RTP 2", "SIC", "TVI",
    "SIC Notícias", "RTP Notícias", "CNN Portugal", "CMTV", "NEWS NOW",
    "GLOBO", "Canal 11", "V+ TVI",
    "SIC Caras", "SIC Radical", "SIC Mulher", "RTP Memória",
    "Sport TV +", "Sport TV 1", "Sport TV 2", "Sport TV 3", "Sport TV 4", "Sport TV 5", "Sport TV 6", "Sport TV 7",
    "Eurosport 1", "Eurosport 2",
    "A BOLA TV", "BTV", "Sporting TV",
    "DAZN 1", "DAZN 2", "DAZN 3", "DAZN 4", "DAZN 5",
    "Porto Canal", "W-Sport", "Fight Network", "Fightbox", "Nautical Channel", "Fuel TV",
    "Disney Channel", "Disney Junior", "Panda Kids", "Cartoon Network", "Panda", "Baby TV",
    "SIC K", "Cartoonito", "Nickelodeon", "Super RTL", "Panda +",
    "VinTv", "Pack Playtime", "Cine Kids", "Dizi",
    "SIC Novelas",
    "TVCine TOP", "TVCine EDITION", "TVCine EMOTION", "TVCine ACTION",
    "Hollywood", "CINEMUNDO",
    "STAR Movies", "STAR Channel", "AXN", "STAR Life", "STAR Crime", "STAR Comedy", "AXN White", "AXN Movies",
    "SyFy", "AMC", "EuroChannel PT", "OPTO SIC",
    "Disney+", "Apple TV+", "HBO Max", "Netflix", "Amazon Prime",
    "Ind ie World", "Super 8", "Pack Playmotion", "M-Cine", "FILMIN", "Canal de Trailers",
    "YouTube", "Canal Q", "MTV Portugal",
    "Stingray Classica", "Stingray Djazz", "MCM TOP", "MCM POP", "C Music", "MEZZO",
    "Afro Music", "Trace Urban", "Trace Toca", "Mezzo Live",
    "24 Kitchen", "Casa e Cozinha", "Food Network",
    "Discovery", "National Geographic", "National Geographic Wild", "História", "Odisseia", "Docubox",
    "AMC Crime", "S+", "ID - Investigation Discovery", "TVI Reality", "TV Record", "AMC BREAK",
    "TLC", "E! Entertainment", "Travel", "Fashion TV", "HGTV", "M6",
    "FastnFunBox", "Ginx Esports TV", "Luxe TV", "Insight TV", "FUNBOX", "Stingray Naturescape",
    "My Zen", "Gametoon", "AR TV",
    "Record News", "CNN", "Euronews PT", "Euronews English", "Bloomberg", "Sky News",
    "BBC World", "CNBC", "Al Jazeera English", "RAI NEWS", "TVE 24h",
    "DW (English)", "France 24 FR", "France 24 English", "TVC News", "Phoenix InfoNews",
    "RTP Madeira", "RTP Açores", "Localvisão", "RTP África", "TPA", "TCV Internacional",
    "Canal 180", "TV Galicia", "TVEi",
    "TV5 Monde", "RAI 1", "VOX", "RTL", "Russia Today Doc", "Russia Today",
    "PRO TV International", "KBS World", "Arirang TV", "NHK World", "Cubavision", "UA | TV",
    "SET Asia", "Utsav Plus", "SET MAX", "Utsav Gold",
    "Canção Nova", "UNIFÉ TV", "Kuriakos TV",
    "Caça&Pesca", "Caçavision", "DOG TV", "OneToro",
    "OH YES!", "Playboy TV", "Hustler TV", "Blue Hustler", "HOT", "Penthouse Passion", "Penthouse Gold",
    "HOT MAN", "HOT TABOO", "PlayBoy",
    "BenficaTV Multicâmara", "Biggs", "EuroChannel FR", "DAZN 6", "Conta Lá",
];

static RANK: Lazy<HashMap<String, usize>> = Lazy::new(|| {
    let mut m = HashMap::new();
    for (i, name) in ORDER.iter().enumerate() {
        let k = canonical_key(name);
        if !k.is_empty() {
            m.entry(k).or_insert(i);
        }
    }
    m
});

pub fn rank(channel_key: &str) -> Option<usize> {
    RANK.get(channel_key).copied()
}
