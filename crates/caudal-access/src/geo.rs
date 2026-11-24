//! Wraps a MaxMind DB (GeoIP2/GeoLite2/DB-IP `.mmdb`) reader for country
//! lookups. The operator supplies the file; none is bundled or downloaded
//! (see `[access] geoip_db` in `caudal.example.toml`).

use std::net::IpAddr;
use std::path::Path;

pub(crate) struct GeoDb {
    reader: maxminddb::Reader<Vec<u8>>,
}

impl GeoDb {
    pub(crate) fn open(path: &Path) -> Result<Self, String> {
        let reader = maxminddb::Reader::open_readfile(path)
            .map_err(|e| format!("opening geoip database {}: {e}", path.display()))?;
        Ok(Self { reader })
    }

    #[cfg(test)]
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        maxminddb::Reader::from_source(bytes).map(|reader| Self { reader }).map_err(|e| e.to_string())
    }

    /// Two-letter ISO 3166-1 alpha-2 country code for `ip`, upper-cased, or
    /// `None` when the database has no record for it (private/reserved
    /// ranges, or a block the database doesn't cover).
    pub(crate) fn country(&self, ip: IpAddr) -> Option<String> {
        let result = self.reader.lookup(ip).ok()?;
        let record: maxminddb::geoip2::Country = result.decode().ok()??;
        record.country.iso_code.map(str::to_ascii_uppercase)
    }

    /// Approximate latitude and longitude (degrees) for `ip`. Only City
    /// databases (GeoLite2-City, DB-IP City) carry coordinates; a Country
    /// database answers `None` for every address.
    pub(crate) fn location(&self, ip: IpAddr) -> Option<(f64, f64)> {
        let result = self.reader.lookup(ip).ok()?;
        let record: maxminddb::geoip2::City = result.decode().ok()??;
        Some((record.location.latitude?, record.location.longitude?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiny_country_db() -> Vec<u8> {
        use maxminddb_writer::Database;
        use maxminddb_writer::paths::IpAddrWithMask;
        use serde::Serialize;

        #[derive(Serialize)]
        struct Rec<'a> {
            country: RecCountry<'a>,
        }
        #[derive(Serialize)]
        struct RecCountry<'a> {
            iso_code: &'a str,
        }

        let mut db = Database::default();
        db.metadata.binary_format_major_version = 2;
        db.metadata.database_type = "GeoIP2-Country-Test".to_owned();
        let pr = db.insert_value(Rec { country: RecCountry { iso_code: "PR" } }).unwrap();
        let de = db.insert_value(Rec { country: RecCountry { iso_code: "DE" } }).unwrap();
        db.insert_node("24.0.0.0/8".parse::<IpAddrWithMask>().unwrap(), pr);
        db.insert_node("81.0.0.0/8".parse::<IpAddrWithMask>().unwrap(), de);
        db.write_to(Vec::new()).unwrap()
    }

    #[test]
    fn looks_up_a_known_country() {
        let db = GeoDb::from_bytes(tiny_country_db()).unwrap();
        assert_eq!(db.country("24.1.2.3".parse().unwrap()), Some("PR".to_owned()));
        assert_eq!(db.country("81.9.9.9".parse().unwrap()), Some("DE".to_owned()));
    }

    #[test]
    fn unknown_address_has_no_country() {
        let db = GeoDb::from_bytes(tiny_country_db()).unwrap();
        assert_eq!(db.country("1.1.1.1".parse().unwrap()), None);
    }

    #[test]
    fn location_needs_a_city_database() {
        use maxminddb_writer::Database;
        use maxminddb_writer::paths::IpAddrWithMask;
        use serde::Serialize;

        #[derive(Serialize)]
        struct Rec<'a> {
            country: RecCountry<'a>,
            location: Loc,
        }
        #[derive(Serialize)]
        struct RecCountry<'a> {
            iso_code: &'a str,
        }
        #[derive(Serialize)]
        struct Loc {
            latitude: f64,
            longitude: f64,
        }
        let mut db = Database::default();
        db.metadata.binary_format_major_version = 2;
        db.metadata.database_type = "GeoIP2-City-Test".to_owned();
        let sj = db
            .insert_value(Rec {
                country: RecCountry { iso_code: "PR" },
                location: Loc { latitude: 18.4655, longitude: -66.1057 },
            })
            .unwrap();
        db.insert_node("24.0.0.0/8".parse::<IpAddrWithMask>().unwrap(), sj);
        let city = GeoDb::from_bytes(db.write_to(Vec::new()).unwrap()).unwrap();
        assert_eq!(city.location("24.1.2.3".parse().unwrap()), Some((18.4655, -66.1057)));
        assert_eq!(city.country("24.1.2.3".parse().unwrap()), Some("PR".to_owned()));
        assert_eq!(city.location("1.1.1.1".parse().unwrap()), None);

        let country_only = GeoDb::from_bytes(tiny_country_db()).unwrap();
        assert_eq!(country_only.location("24.1.2.3".parse().unwrap()), None);
    }

    #[test]
    fn bad_path_is_an_error() {
        assert!(GeoDb::open(Path::new("/nonexistent/geo.mmdb")).is_err());
    }
}
