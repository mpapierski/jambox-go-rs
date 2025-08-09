// Diesel schema (manually authored). Run `diesel print-schema` if using CLI later.
diesel::table! {
    channels (id) {
        id -> Integer,
        sgtid -> Integer,
        name -> Text,
        url -> Text,
    }
}
