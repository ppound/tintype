//! Auto tags: what a photo shows ("Dog", "Beach", "Castle"), found by comparing
//! its smart search embedding with a fixed list of labels. No extra pass over
//! the photos is needed, and the tags are kept apart from the user's own.
//!
//! Each label has its own threshold: SigLIP scores some prompts higher than
//! others across the board. They were set by looking at photos just above and
//! below them in a library of ~36,000 photos, preferring missed tags to wrong
//! ones. Labels it couldn't tell apart reliably (sunrise from sunset, selfies
//! from portraits) are left out.

use crate::{Result, TextEncoder, similarity};

/// Bump when labels, prompts or thresholds change; photos are then tagged again.
pub const AUTO_TAG_VERSION: u32 = 1;
/// At most this many tags per photo, the clearest first.
const MAX_TAGS: usize = 6;

/// One label: its group (the first level of the Subjects filter), its name, and
/// the prompt it is matched with ("a photo of a {name}." when empty).
pub struct Label {
    pub group: &'static str,
    pub name: &'static str,
    prompt: &'static str,
    threshold: f32,
}

const fn label(group: &'static str, name: &'static str, prompt: &'static str, threshold: f32) -> Label {
    Label { group, name, prompt, threshold }
}

impl Label {
    fn prompt(&self) -> String {
        if !self.prompt.is_empty() {
            return self.prompt.to_string();
        }
        let name = self.name.to_lowercase();
        let article = if name.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" };
        format!("a photo of {article} {name}.")
    }

    /// "Animals/Dog": how a tag is stored and filtered on.
    pub fn path(&self) -> String {
        format!("{}/{}", self.group, self.name)
    }
}

pub const LABELS: &[Label] = &[
    label("Animals", "Dog", "", 0.060),
    label("Animals", "Cat", "", 0.085),
    label("Animals", "Bird", "", 0.075),
    label("Animals", "Horse", "", 0.080),
    label("Animals", "Cow", "a photo of cows.", 0.080),
    label("Animals", "Sheep", "a photo of sheep.", 0.080),
    label("Animals", "Duck", "", 0.080),
    label("Animals", "Swan", "", 0.080),
    label("Animals", "Seagull", "", 0.080),
    label("Animals", "Peacock", "", 0.080),
    label("Animals", "Deer", "", 0.080),
    label("Animals", "Fish", "", 0.080),
    label("Animals", "Butterfly", "", 0.080),
    label("Animals", "Insect", "", 0.080),
    label("Animals", "Squirrel", "", 0.080),
    label("Animals", "Rabbit", "", 0.080),
    label("Animals", "Chicken", "", 0.080),
    label("Animals", "Pig", "", 0.080),
    label("Animals", "Goat", "", 0.080),
    label("Animals", "Lizard", "", 0.080),
    label("Animals", "Seal", "", 0.080),
    label("Animals", "Zoo animal", "a photo of an animal at the zoo.", 0.080),
    label("People", "Portrait", "a portrait photo of a person.", 0.070),
    label("People", "Group photo", "a group photo of people posing together.", 0.085),
    label("People", "Crowd", "a photo of a crowd of people.", 0.085),
    label("People", "Child", "", 0.090),
    label("People", "Baby", "", 0.080),
    label("Nature", "Beach", "", 0.080),
    label("Nature", "Mountains", "a photo of mountains.", 0.080),
    label("Nature", "Forest", "", 0.080),
    label("Nature", "Lake", "", 0.085),
    label("Nature", "River", "", 0.085),
    label("Nature", "Waterfall", "", 0.080),
    label("Nature", "Field", "", 0.080),
    label("Nature", "Desert", "", 0.080),
    label("Nature", "Cliffs", "a photo of cliffs.", 0.090),
    label("Nature", "Island", "", 0.090),
    label("Nature", "Snow", "a photo of a snowy landscape.", 0.070),
    label("Nature", "Sky", "a photo of the sky and clouds.", 0.080),
    label("Nature", "Sunset", "", 0.075),
    label("Nature", "Rainbow", "", 0.080),
    label("Nature", "Fog", "a photo of a foggy landscape.", 0.080),
    label("Nature", "Night sky", "a photo of the night sky with stars.", 0.080),
    label("Nature", "Moon", "a photo of the moon.", 0.080),
    label("Nature", "Landscape", "a landscape photo.", 0.085),
    label("Plants", "Flower", "", 0.080),
    label("Plants", "Tree", "", 0.080),
    label("Plants", "Garden", "", 0.085),
    label("Plants", "Autumn leaves", "a photo of autumn leaves.", 0.080),
    label("Plants", "Mushroom", "", 0.080),
    label("Places", "City", "", 0.080),
    label("Places", "Street", "", 0.090),
    label("Places", "Village", "", 0.090),
    label("Places", "Harbor", "a photo of a harbor with boats.", 0.085),
    label("Places", "Park", "", 0.085),
    label("Places", "Market", "", 0.085),
    label("Places", "Restaurant", "a photo inside a restaurant.", 0.080),
    label("Places", "Museum", "a photo inside a museum.", 0.080),
    label("Places", "Shop", "", 0.090),
    label("Places", "Airport", "", 0.080),
    label("Places", "Train station", "", 0.080),
    label("Places", "Playground", "", 0.080),
    label("Places", "Stadium", "", 0.080),
    label("Places", "Swimming pool", "", 0.080),
    label("Places", "Kitchen", "", 0.070),
    label("Places", "Living room", "", 0.080),
    label("Places", "Bedroom", "", 0.080),
    label("Places", "Office", "", 0.080),
    label("Places", "Classroom", "", 0.080),
    label("Buildings", "Castle", "", 0.090),
    label("Buildings", "Church", "", 0.080),
    label("Buildings", "Cathedral", "", 0.080),
    label("Buildings", "Temple", "", 0.080),
    label("Buildings", "Ruins", "a photo of ancient ruins.", 0.090),
    label("Buildings", "Bridge", "", 0.080),
    label("Buildings", "Tower", "", 0.085),
    label("Buildings", "Lighthouse", "", 0.080),
    label("Buildings", "Skyscraper", "a photo of skyscrapers.", 0.080),
    label("Buildings", "House", "", 0.085),
    label("Buildings", "Windmill", "", 0.080),
    label("Buildings", "Monument", "", 0.090),
    label("Buildings", "Statue", "", 0.080),
    label("Buildings", "Fountain", "", 0.080),
    label("Buildings", "Stained glass", "a photo of a stained glass window.", 0.080),
    label("Buildings", "Architecture", "an architecture photo of a building.", 0.080),
    label("Vehicles", "Car", "", 0.080),
    label("Vehicles", "Boat", "", 0.080),
    label("Vehicles", "Ship", "", 0.080),
    label("Vehicles", "Train", "", 0.080),
    label("Vehicles", "Airplane", "", 0.080),
    label("Vehicles", "Bicycle", "", 0.080),
    label("Vehicles", "Motorcycle", "", 0.080),
    label("Vehicles", "Bus", "", 0.080),
    label("Vehicles", "Tram", "", 0.080),
    label("Vehicles", "Truck", "", 0.080),
    label("Food & drink", "Food", "a photo of food.", 0.080),
    label("Food & drink", "Cake", "", 0.080),
    label("Food & drink", "Pizza", "", 0.080),
    label("Food & drink", "Bread", "", 0.080),
    label("Food & drink", "Fruit", "a photo of fruit.", 0.080),
    label("Food & drink", "Vegetables", "a photo of vegetables.", 0.080),
    label("Food & drink", "Coffee", "a photo of a cup of coffee.", 0.080),
    label("Food & drink", "Wine", "a photo of wine.", 0.080),
    label("Food & drink", "Beer", "a photo of beer.", 0.080),
    label("Food & drink", "Ice cream", "", 0.080),
    label("Food & drink", "Seafood", "a photo of seafood.", 0.080),
    label("Events", "Birthday", "a photo of a birthday party.", 0.080),
    label("Events", "Wedding", "", 0.080),
    label("Events", "Christmas", "a photo of christmas.", 0.080),
    label("Events", "Concert", "", 0.090),
    label("Events", "Party", "", 0.090),
    label("Events", "Graduation", "", 0.090),
    label("Events", "Fireworks", "a photo of fireworks.", 0.080),
    label("Events", "Parade", "", 0.090),
    label("Activities", "Hiking", "a photo of people hiking.", 0.085),
    label("Activities", "Swimming", "a photo of people swimming.", 0.090),
    label("Activities", "Skiing", "a photo of people skiing.", 0.080),
    label("Activities", "Cycling", "a photo of people cycling.", 0.080),
    label("Activities", "Camping", "a photo of camping.", 0.090),
    label("Activities", "Soccer", "a photo of people playing soccer.", 0.080),
    label("Activities", "Fishing", "a photo of people fishing.", 0.090),
    label("Activities", "Sailing", "a photo of sailing.", 0.080),
    label("Activities", "Kayaking", "a photo of people kayaking.", 0.080),
    label("Activities", "Cooking", "a photo of people cooking.", 0.080),
    label("Things", "Painting", "", 0.080),
    label("Things", "Sculpture", "", 0.080),
    label("Things", "Book", "a photo of books.", 0.080),
    label("Things", "Toy", "a photo of toys.", 0.080),
    label("Things", "Musical instrument", "", 0.075),
    label("Things", "Sign", "", 0.080),
    label("Things", "Graffiti", "", 0.080),
    label("Things", "Christmas tree", "", 0.080),
    label("Things", "Candle", "a photo of candles.", 0.080),
    label("Things", "Clock", "", 0.080),
    label("Things", "Umbrella", "", 0.080),
    label("Things", "Balloons", "a photo of balloons.", 0.080),
    label("Things", "Computer", "", 0.080),
    label("Things", "Phone", "a photo of a mobile phone.", 0.080),
    label("Documents", "Screenshot", "a screenshot of a computer screen.", 0.080),
    label("Documents", "Document", "a photo of a document with text.", 0.080),
    label("Documents", "Receipt", "", 0.080),
    label("Documents", "Map", "", 0.080),
    label("Documents", "Whiteboard", "", 0.080),
    label("Style", "Black and white", "a black and white photo.", 0.070),
    label("Style", "Night", "a photo taken at night.", 0.085),
    label("Style", "Aerial view", "an aerial photo taken from above.", 0.080),
    label("Style", "Close-up", "a macro close-up photo.", 0.080),
    label("Style", "Underwater", "an underwater photo.", 0.080),
    label("Style", "Reflection", "a photo of a reflection in water.", 0.080),
    label("Style", "Silhouette", "a silhouette photo.", 0.080),
];

/// Tags photos from their embeddings. Embedding the labels takes a few seconds,
/// so make one and keep it.
pub struct AutoTagger {
    labels: Vec<Vec<f32>>,
}

impl AutoTagger {
    pub fn new(text: &TextEncoder) -> Result<Self> {
        let labels = LABELS.iter().map(|l| text.embed(&l.prompt())).collect::<Result<_>>()?;
        Ok(Self { labels })
    }

    /// The tags for a photo's embedding, as [`Label::path`]s, the clearest first.
    pub fn tags(&self, embedding: &[f32]) -> Vec<String> {
        let mut hits: Vec<(f32, &Label)> = LABELS
            .iter()
            .zip(&self.labels)
            .map(|(l, v)| (similarity(embedding, v) - l.threshold, l))
            .filter(|(margin, _)| *margin >= 0.0)
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        hits.into_iter().take(MAX_TAGS).map(|(_, l)| l.path()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_unique_and_well_formed() {
        let mut names: Vec<&str> = LABELS.iter().map(|l| l.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), LABELS.len(), "a label name is used twice");
        for l in LABELS {
            assert!(!l.name.contains('/') && !l.group.contains('/'), "{} would split the facet path", l.name);
            assert!((0.05..0.1).contains(&l.threshold), "{} threshold {}", l.name, l.threshold);
        }
        let dog = LABELS.iter().find(|l| l.name == "Dog").unwrap();
        assert_eq!((dog.prompt(), dog.path()), ("a photo of a dog.".to_string(), "Animals/Dog".to_string()));
        let owl = label("Animals", "Owl", "", 0.08);
        assert_eq!(owl.prompt(), "a photo of an owl.");
    }

    #[test]
    fn tags_clear_their_threshold_clearest_first() {
        let n = LABELS.len();
        // Fake label vectors: label i is the unit vector e_i.
        let tagger =
            AutoTagger { labels: (0..n).map(|i| (0..n).map(|j| f32::from(u8::from(i == j))).collect()).collect() };
        let mut photo = vec![0.0; n];
        photo[0] = LABELS[0].threshold + 0.01;
        photo[1] = LABELS[1].threshold + 0.03;
        photo[2] = LABELS[2].threshold - 0.001;
        assert_eq!(tagger.tags(&photo), [LABELS[1].path(), LABELS[0].path()]);
        assert_eq!(tagger.tags(&vec![1.0; n]).len(), MAX_TAGS);
    }
}
