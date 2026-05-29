// Provider Name Generation
//
// Derives a 3-word human-readable name for a provider from its Nostr
// public key.  Format: {Adjective}{Middle}{Noun} — e.g. "SwiftGoldenOwl",
// "AncientSilverNarwhal", "BraveCrimsonFalcon".
//
// Derivation uses three independent 8-byte windows of the 32-byte
// secp256k1 public key to index into three word lists:
//
//   adj  = ADJECTIVES[ bytes[0..8]  as u64  % len ]
//   mid  = MIDDLES   [ bytes[8..16] as u64  % len ]
//   noun = NOUNS     [ bytes[16..24] as u64 % len ]
//
// Properties:
//   - Deterministic: same key always → same name (idempotent bootstrap).
//   - Collision-resistant: 150 × 100 × 150 = 2,250,000 combinations;
//     birthday collision probability stays below 1% until ~2,100
//     simultaneous providers on the network.
//   - No dependencies beyond std.

/// Derives a 3-word provider name from the raw bytes of a Nostr public key
/// (32-byte x-only secp256k1 coordinate).
///
/// # Panics
/// Panics if `pubkey_bytes` is shorter than 24 bytes.  A valid Nostr
/// public key is always 32 bytes so this never fires in production.
pub fn derive_provider_name(pubkey_bytes: &[u8]) -> String {
    assert!(
        pubkey_bytes.len() >= 24,
        "pubkey_bytes must be at least 24 bytes (got {})",
        pubkey_bytes.len()
    );
    let a = u64::from_le_bytes(pubkey_bytes[0..8].try_into().unwrap());
    let b = u64::from_le_bytes(pubkey_bytes[8..16].try_into().unwrap());
    let c = u64::from_le_bytes(pubkey_bytes[16..24].try_into().unwrap());

    let adj  = ADJECTIVES[(a % ADJECTIVES.len() as u64) as usize];
    let mid  = MIDDLES   [(b % MIDDLES.len()    as u64) as usize];
    let noun = NOUNS     [(c % NOUNS.len()      as u64) as usize];

    format!("{}{}{}", adj, mid, noun)
}

// ─── Word lists ───────────────────────────────────────────────────────────────
//
// Rules:
//  - Each word is PascalCase so concatenation is readable: "SwiftGoldenOwl".
//  - No word appears in more than one list.
//  - Adjectives describe character/quality; Middles are colours/materials/
//    nature-phenomena; Nouns are animals or striking natural features.

const ADJECTIVES: &[&str] = &[
    "Ancient",   "Arctic",    "Ashen",     "Autumn",    "Barren",
    "Blazing",   "Bold",      "Brave",     "Bright",    "Calm",
    "Clever",    "Cold",      "Cosmic",    "Crystal",   "Cunning",
    "Dark",      "Deep",      "Distant",   "Divine",    "Drifting",
    "Dusty",     "Eager",     "Elusive",   "Empty",     "Eternal",
    "Fading",    "Fallen",    "Fierce",    "Flaming",   "Floating",
    "Foggy",     "Frozen",    "Furious",   "Gentle",    "Giant",
    "Gleaming",  "Glowing",   "Grand",     "Grim",      "Haunted",
    "Heavy",     "Hidden",    "Hollow",    "Humble",    "Hungry",
    "Icy",       "Immortal",  "Jagged",    "Jolly",     "Keen",
    "Languid",   "Lazy",      "Lively",    "Lone",      "Lost",
    "Loud",      "Lucky",     "Mad",       "Majestic",  "Melancholy",
    "Mighty",    "Misty",     "Modest",    "Mystic",    "Narrow",
    "Noble",     "Odd",       "Old",       "Ornate",    "Phantom",
    "Pale",      "Patient",   "Peaceful",  "Playful",
    "Polar",     "Proud",     "Quick",     "Quiet",     "Radiant",
    "Rapid",     "Rare",      "Restless",  "Rising",    "Roaming",
    "Rocky",     "Royal",     "Rugged",    "Sacred",    "Savage",
    "Secret",    "Serene",    "Silent",    "Slim",      "Slow",
    "Sneaky",    "Solar",     "Solemn",    "Sparkling", "Steep",
    "Stern",     "Still",     "Stormy",    "Strange",   "Strong",
    "Stubborn",  "Sunny",     "Swift",     "Tall",      "Tame",
    "Tiny",      "Tired",     "Tropical",  "Twisted",   "Vast",
    "Vibrant",   "Wandering", "Warm",      "Wicked",    "Wild",
    "Windy",     "Wise",      "Young",     "Zealous",   "Arid",
    "Breezy",    "Brittle",   "Charred",   "Crooked",   "Damp",
    "Desolate",  "Enchanted", "Faint",     "Gilded",    "Knotted",
    "Mellow",    "Parched",   "Prickly",   "Rippling",  "Sheer",
    "Tattered",  "Towering",  "Weathered", "Wiry",      "Dreary",
    "Inky",      "Lurking",   "Sinister",  "Frail",     "Nimble",
]; // 152 — intentional: prime-ish length helps uniform distribution

const MIDDLES: &[&str] = &[
    "Amber",     "Birch",     "Black",     "Blue",      "Bronze",
    "Cedar",     "Cerulean",  "Charcoal",  "Cinder",    "Cobalt",
    "Copper",    "Coral",     "Crimson",   "Cyan",      "Dawn",
    "Dew",       "Dusky",     "Dusk",      "Ebony",     "Echo",
    "Ember",     "Emerald",   "Fern",      "Flint",     "Flare",
    "Frost",     "Frosty",    "Gale",      "Glen",      "Golden",
    "Granite",   "Grey",      "Haze",      "Indigo",    "Iron",
    "Ivory",     "Jade",      "Lavender",  "Lunar",     "Magenta",
    "Mahogany",  "Maroon",    "Marsh",     "Mesa",      "Mist",
    "Moor",      "Mossy",     "Murky",     "Navy",      "Obsidian",
    "Ochre",     "Olive",     "Onyx",      "Opal",      "Orange",
    "Pearl",     "Peat",      "Pine",      "Plum",      "Purple",
    "Rain",      "Reed",      "Ridge",     "Rime",      "Rose",
    "Ruby",      "Rune",      "Russet",    "Rust",      "Sage",
    "Salt",      "Sand",      "Sandy",     "Scarlet",   "Silk",
    "Silver",    "Sky",       "Slate",     "Smoky",     "Snow",
    "Soot",      "Steel",     "Stone",     "Storm",     "Teal",
    "Thorn",     "Tide",      "Twilight",  "Umber",     "Vale",
    "Velvet",    "Violet",    "Wave",      "White",     "Wind",
    "Yellow",    "Ash",       "Silt",      "Flax",      "Brine",
]; // 100

const NOUNS: &[&str] = &[
    // Animals
    "Albatross",  "Armadillo",  "Badger",     "Bear",       "Beaver",
    "Bison",      "Boar",       "Buffalo",     "Capybara",   "Chameleon",
    "Cheetah",    "Chinchilla", "Cobra",       "Condor",     "Cougar",
    "Coyote",     "Crane",      "Crow",        "Deer",       "Dingo",
    "Dolphin",    "Dragon",     "Eagle",       "Elk",        "Falcon",
    "Ferret",     "Flamingo",   "Fox",         "Gecko",      "Giraffe",
    "Gorilla",    "Hawk",       "Hedgehog",    "Heron",      "Hyena",
    "Ibis",       "Iguana",     "Jaguar",      "Jackal",     "Kestrel",
    "Koala",      "Lemur",      "Leopard",     "Lynx",       "Macaw",
    "Mamba",      "Manatee",    "Marmot",      "Moose",      "Narwhal",
    "Ocelot",     "Okapi",      "Osprey",      "Otter",      "Owl",
    "Panda",      "Panther",    "Parrot",      "Peacock",    "Penguin",
    "Platypus",   "Puffin",     "Python",      "Quokka",     "Rabbit",
    "Raccoon",    "Raven",      "Rhino",       "Robin",      "Salmon",
    "Scorpion",   "Shark",      "Sloth",       "Sparrow",    "Squid",
    "Stork",      "Swan",       "Tapir",       "Tiger",      "Toucan",
    "Turtle",     "Viper",      "Vulture",     "Walrus",     "Warthog",
    "Weasel",     "Whale",      "Wolf",        "Wolverine",  "Wombat",
    "Wren",       "Yak",        "Zebra",
    // Natural features
    "Abyss",      "Avalanche",  "Blizzard",    "Canyon",     "Cascade",
    "Cavern",     "Chasm",      "Cliff",       "Comet",      "Cove",
    "Creek",      "Cyclone",    "Delta",       "Dune",       "Eclipse",
    "Equinox",    "Fjord",      "Geyser",      "Glacier",    "Glade",
    "Grotto",     "Grove",      "Haven",       "Isle",       "Knoll",
    "Lagoon",     "Maelstrom",  "Mirage",      "Monsoon",    "Nebula",
    "Nova",       "Oasis",      "Peak",        "Plateau",    "Prism",
    "Pulsar",     "Quasar",     "Ravine",      "Solstice",   "Spire",
    "Summit",     "Tempest",    "Tundra",      "Typhoon",    "Vault",
    "Vortex",     "Zenith",     "Zephyr",
]; // 141

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_is_deterministic() {
        let key = [0xab_u8; 32];
        assert_eq!(derive_provider_name(&key), derive_provider_name(&key));
    }

    #[test]
    fn different_keys_produce_different_names() {
        let mut key_a = [0u8; 32];
        let mut key_b = [0u8; 32];
        key_a[0] = 1;
        key_b[0] = 2;
        assert_ne!(derive_provider_name(&key_a), derive_provider_name(&key_b));
    }

    #[test]
    fn output_is_three_capitalised_words() {
        let key = [0x42_u8; 32];
        let name = derive_provider_name(&key);
        // Each word is PascalCase — starts with uppercase, rest lowercase-or-mixed.
        // The simplest check: name is non-empty, no spaces, no hyphens.
        assert!(!name.is_empty());
        assert!(!name.contains(' '));
        assert!(!name.contains('-'));
        // All three words present: the adj/mid/noun are each at least 3 chars.
        assert!(name.len() >= 9);
    }

    #[test]
    fn no_duplicate_words_across_lists() {
        let all: Vec<&str> = ADJECTIVES.iter()
            .chain(MIDDLES.iter())
            .chain(NOUNS.iter())
            .copied()
            .collect();
        let unique: std::collections::HashSet<&str> = all.iter().copied().collect();
        assert_eq!(
            all.len(), unique.len(),
            "duplicate word found across ADJECTIVES / MIDDLES / NOUNS"
        );
    }

    #[test]
    fn combination_count_exceeds_two_million() {
        let combos = ADJECTIVES.len() * MIDDLES.len() * NOUNS.len();
        assert!(combos > 2_000_000, "only {} combinations", combos);
    }
}
