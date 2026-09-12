//! Country centroids: ISO 3166-1 alpha-2 code → (latitude, longitude).
//!
//! Generated once from Google's canonical `countries.csv`
//! (<https://developers.google.com/public-data/docs/canonical/countries_csv>,
//! CC BY 4.0), rounded to 4 decimals. Sorted by code for binary search — a
//! centroid is where GLOBE's arc lands for a connection into that country.

/// `(code, lat, lon)`, sorted by `code`.
pub const CENTROIDS: &[(&str, f64, f64)] = &[
    ("AD", 42.5462, 1.6016), // Andorra
    ("AE", 23.4241, 53.8478), // United Arab Emirates
    ("AF", 33.9391, 67.7100), // Afghanistan
    ("AG", 17.0608, -61.7964), // Antigua and Barbuda
    ("AI", 18.2206, -63.0686), // Anguilla
    ("AL", 41.1533, 20.1683), // Albania
    ("AM", 40.0691, 45.0382), // Armenia
    ("AN", 12.2261, -69.0601), // Netherlands Antilles
    ("AO", -11.2027, 17.8739), // Angola
    ("AQ", -75.2510, -0.0714), // Antarctica
    ("AR", -38.4161, -63.6167), // Argentina
    ("AS", -14.2710, -170.1322), // American Samoa
    ("AT", 47.5162, 14.5501), // Austria
    ("AU", -25.2744, 133.7751), // Australia
    ("AW", 12.5211, -69.9683), // Aruba
    ("AZ", 40.1431, 47.5769), // Azerbaijan
    ("BA", 43.9159, 17.6791), // Bosnia and Herzegovina
    ("BB", 13.1939, -59.5432), // Barbados
    ("BD", 23.6850, 90.3563), // Bangladesh
    ("BE", 50.5039, 4.4699), // Belgium
    ("BF", 12.2383, -1.5616), // Burkina Faso
    ("BG", 42.7339, 25.4858), // Bulgaria
    ("BH", 25.9304, 50.6378), // Bahrain
    ("BI", -3.3731, 29.9189), // Burundi
    ("BJ", 9.3077, 2.3158), // Benin
    ("BM", 32.3214, -64.7574), // Bermuda
    ("BN", 4.5353, 114.7277), // Brunei
    ("BO", -16.2902, -63.5887), // Bolivia
    ("BR", -14.2350, -51.9253), // Brazil
    ("BS", 25.0343, -77.3963), // Bahamas
    ("BT", 27.5142, 90.4336), // Bhutan
    ("BV", -54.4232, 3.4132), // Bouvet Island
    ("BW", -22.3285, 24.6849), // Botswana
    ("BY", 53.7098, 27.9534), // Belarus
    ("BZ", 17.1899, -88.4976), // Belize
    ("CA", 56.1304, -106.3468), // Canada
    ("CC", -12.1642, 96.8710), // Cocos [Keeling] Islands
    ("CD", -4.0383, 21.7587), // Congo [DRC]
    ("CF", 6.6111, 20.9394), // Central African Republic
    ("CG", -0.2280, 15.8277), // Congo [Republic]
    ("CH", 46.8182, 8.2275), // Switzerland
    ("CI", 7.5400, -5.5471), // Côte d'Ivoire
    ("CK", -21.2367, -159.7777), // Cook Islands
    ("CL", -35.6751, -71.5430), // Chile
    ("CM", 7.3697, 12.3547), // Cameroon
    ("CN", 35.8617, 104.1954), // China
    ("CO", 4.5709, -74.2973), // Colombia
    ("CR", 9.7489, -83.7534), // Costa Rica
    ("CU", 21.5218, -77.7812), // Cuba
    ("CV", 16.0021, -24.0132), // Cape Verde
    ("CX", -10.4475, 105.6904), // Christmas Island
    ("CY", 35.1264, 33.4299), // Cyprus
    ("CZ", 49.8175, 15.4730), // Czech Republic
    ("DE", 51.1657, 10.4515), // Germany
    ("DJ", 11.8251, 42.5903), // Djibouti
    ("DK", 56.2639, 9.5018), // Denmark
    ("DM", 15.4150, -61.3710), // Dominica
    ("DO", 18.7357, -70.1627), // Dominican Republic
    ("DZ", 28.0339, 1.6596), // Algeria
    ("EC", -1.8312, -78.1834), // Ecuador
    ("EE", 58.5953, 25.0136), // Estonia
    ("EG", 26.8206, 30.8025), // Egypt
    ("EH", 24.2155, -12.8858), // Western Sahara
    ("ER", 15.1794, 39.7823), // Eritrea
    ("ES", 40.4637, -3.7492), // Spain
    ("ET", 9.1450, 40.4897), // Ethiopia
    ("FI", 61.9241, 25.7482), // Finland
    ("FJ", -16.5782, 179.4144), // Fiji
    ("FK", -51.7963, -59.5236), // Falkland Islands [Islas Malvinas]
    ("FM", 7.4256, 150.5508), // Micronesia
    ("FO", 61.8926, -6.9118), // Faroe Islands
    ("FR", 46.2276, 2.2137), // France
    ("GA", -0.8037, 11.6094), // Gabon
    ("GB", 55.3781, -3.4360), // United Kingdom
    ("GD", 12.2628, -61.6042), // Grenada
    ("GE", 42.3154, 43.3569), // Georgia
    ("GF", 3.9339, -53.1258), // French Guiana
    ("GG", 49.4657, -2.5853), // Guernsey
    ("GH", 7.9465, -1.0232), // Ghana
    ("GI", 36.1377, -5.3454), // Gibraltar
    ("GL", 71.7069, -42.6043), // Greenland
    ("GM", 13.4432, -15.3101), // Gambia
    ("GN", 9.9456, -9.6966), // Guinea
    ("GP", 16.9960, -62.0676), // Guadeloupe
    ("GQ", 1.6508, 10.2679), // Equatorial Guinea
    ("GR", 39.0742, 21.8243), // Greece
    ("GS", -54.4296, -36.5879), // South Georgia and the South Sandwich Islands
    ("GT", 15.7835, -90.2308), // Guatemala
    ("GU", 13.4443, 144.7937), // Guam
    ("GW", 11.8037, -15.1804), // Guinea-Bissau
    ("GY", 4.8604, -58.9302), // Guyana
    ("GZ", 31.3547, 34.3088), // Gaza Strip
    ("HK", 22.3964, 114.1095), // Hong Kong
    ("HM", -53.0818, 73.5042), // Heard Island and McDonald Islands
    ("HN", 15.2000, -86.2419), // Honduras
    ("HR", 45.1000, 15.2000), // Croatia
    ("HT", 18.9712, -72.2852), // Haiti
    ("HU", 47.1625, 19.5033), // Hungary
    ("ID", -0.7893, 113.9213), // Indonesia
    ("IE", 53.4129, -8.2439), // Ireland
    ("IL", 31.0461, 34.8516), // Israel
    ("IM", 54.2361, -4.5481), // Isle of Man
    ("IN", 20.5937, 78.9629), // India
    ("IO", -6.3432, 71.8765), // British Indian Ocean Territory
    ("IQ", 33.2232, 43.6793), // Iraq
    ("IR", 32.4279, 53.6880), // Iran
    ("IS", 64.9631, -19.0208), // Iceland
    ("IT", 41.8719, 12.5674), // Italy
    ("JE", 49.2144, -2.1313), // Jersey
    ("JM", 18.1096, -77.2975), // Jamaica
    ("JO", 30.5852, 36.2384), // Jordan
    ("JP", 36.2048, 138.2529), // Japan
    ("KE", -0.0236, 37.9062), // Kenya
    ("KG", 41.2044, 74.7661), // Kyrgyzstan
    ("KH", 12.5657, 104.9910), // Cambodia
    ("KI", -3.3704, -168.7340), // Kiribati
    ("KM", -11.8750, 43.8722), // Comoros
    ("KN", 17.3578, -62.7830), // Saint Kitts and Nevis
    ("KP", 40.3399, 127.5101), // North Korea
    ("KR", 35.9078, 127.7669), // South Korea
    ("KW", 29.3117, 47.4818), // Kuwait
    ("KY", 19.5135, -80.5670), // Cayman Islands
    ("KZ", 48.0196, 66.9237), // Kazakhstan
    ("LA", 19.8563, 102.4955), // Laos
    ("LB", 33.8547, 35.8623), // Lebanon
    ("LC", 13.9094, -60.9789), // Saint Lucia
    ("LI", 47.1660, 9.5554), // Liechtenstein
    ("LK", 7.8731, 80.7718), // Sri Lanka
    ("LR", 6.4281, -9.4295), // Liberia
    ("LS", -29.6100, 28.2336), // Lesotho
    ("LT", 55.1694, 23.8813), // Lithuania
    ("LU", 49.8153, 6.1296), // Luxembourg
    ("LV", 56.8796, 24.6032), // Latvia
    ("LY", 26.3351, 17.2283), // Libya
    ("MA", 31.7917, -7.0926), // Morocco
    ("MC", 43.7503, 7.4128), // Monaco
    ("MD", 47.4116, 28.3699), // Moldova
    ("ME", 42.7087, 19.3744), // Montenegro
    ("MG", -18.7669, 46.8691), // Madagascar
    ("MH", 7.1315, 171.1845), // Marshall Islands
    ("MK", 41.6086, 21.7453), // Macedonia [FYROM]
    ("ML", 17.5707, -3.9962), // Mali
    ("MM", 21.9140, 95.9562), // Myanmar [Burma]
    ("MN", 46.8625, 103.8467), // Mongolia
    ("MO", 22.1987, 113.5439), // Macau
    ("MP", 17.3308, 145.3847), // Northern Mariana Islands
    ("MQ", 14.6415, -61.0242), // Martinique
    ("MR", 21.0079, -10.9408), // Mauritania
    ("MS", 16.7425, -62.1874), // Montserrat
    ("MT", 35.9375, 14.3754), // Malta
    ("MU", -20.3484, 57.5522), // Mauritius
    ("MV", 3.2028, 73.2207), // Maldives
    ("MW", -13.2543, 34.3015), // Malawi
    ("MX", 23.6345, -102.5528), // Mexico
    ("MY", 4.2105, 101.9758), // Malaysia
    ("MZ", -18.6657, 35.5296), // Mozambique
    ("NA", -22.9576, 18.4904), // Namibia
    ("NC", -20.9043, 165.6180), // New Caledonia
    ("NE", 17.6078, 8.0817), // Niger
    ("NF", -29.0408, 167.9547), // Norfolk Island
    ("NG", 9.0820, 8.6753), // Nigeria
    ("NI", 12.8654, -85.2072), // Nicaragua
    ("NL", 52.1326, 5.2913), // Netherlands
    ("NO", 60.4720, 8.4689), // Norway
    ("NP", 28.3949, 84.1240), // Nepal
    ("NR", -0.5228, 166.9315), // Nauru
    ("NU", -19.0544, -169.8672), // Niue
    ("NZ", -40.9006, 174.8860), // New Zealand
    ("OM", 21.5126, 55.9233), // Oman
    ("PA", 8.5380, -80.7821), // Panama
    ("PE", -9.1900, -75.0152), // Peru
    ("PF", -17.6797, -149.4068), // French Polynesia
    ("PG", -6.3150, 143.9555), // Papua New Guinea
    ("PH", 12.8797, 121.7740), // Philippines
    ("PK", 30.3753, 69.3451), // Pakistan
    ("PL", 51.9194, 19.1451), // Poland
    ("PM", 46.9419, -56.2711), // Saint Pierre and Miquelon
    ("PN", -24.7036, -127.4393), // Pitcairn Islands
    ("PR", 18.2208, -66.5901), // Puerto Rico
    ("PS", 31.9522, 35.2332), // Palestinian Territories
    ("PT", 39.3999, -8.2245), // Portugal
    ("PW", 7.5150, 134.5825), // Palau
    ("PY", -23.4425, -58.4438), // Paraguay
    ("QA", 25.3548, 51.1839), // Qatar
    ("RE", -21.1151, 55.5364), // Réunion
    ("RO", 45.9432, 24.9668), // Romania
    ("RS", 44.0165, 21.0059), // Serbia
    ("RU", 61.5240, 105.3188), // Russia
    ("RW", -1.9403, 29.8739), // Rwanda
    ("SA", 23.8859, 45.0792), // Saudi Arabia
    ("SB", -9.6457, 160.1562), // Solomon Islands
    ("SC", -4.6796, 55.4920), // Seychelles
    ("SD", 12.8628, 30.2176), // Sudan
    ("SE", 60.1282, 18.6435), // Sweden
    ("SG", 1.3521, 103.8198), // Singapore
    ("SH", -24.1435, -10.0307), // Saint Helena
    ("SI", 46.1512, 14.9955), // Slovenia
    ("SJ", 77.5536, 23.6703), // Svalbard and Jan Mayen
    ("SK", 48.6690, 19.6990), // Slovakia
    ("SL", 8.4606, -11.7799), // Sierra Leone
    ("SM", 43.9424, 12.4578), // San Marino
    ("SN", 14.4974, -14.4524), // Senegal
    ("SO", 5.1521, 46.1996), // Somalia
    ("SR", 3.9193, -56.0278), // Suriname
    ("ST", 0.1864, 6.6131), // São Tomé and Príncipe
    ("SV", 13.7942, -88.8965), // El Salvador
    ("SY", 34.8021, 38.9968), // Syria
    ("SZ", -26.5225, 31.4659), // Swaziland
    ("TC", 21.6940, -71.7979), // Turks and Caicos Islands
    ("TD", 15.4542, 18.7322), // Chad
    ("TF", -49.2804, 69.3486), // French Southern Territories
    ("TG", 8.6195, 0.8248), // Togo
    ("TH", 15.8700, 100.9925), // Thailand
    ("TJ", 38.8610, 71.2761), // Tajikistan
    ("TK", -8.9674, -171.8559), // Tokelau
    ("TL", -8.8742, 125.7275), // Timor-Leste
    ("TM", 38.9697, 59.5563), // Turkmenistan
    ("TN", 33.8869, 9.5375), // Tunisia
    ("TO", -21.1790, -175.1982), // Tonga
    ("TR", 38.9637, 35.2433), // Turkey
    ("TT", 10.6918, -61.2225), // Trinidad and Tobago
    ("TV", -7.1095, 177.6493), // Tuvalu
    ("TW", 23.6978, 120.9605), // Taiwan
    ("TZ", -6.3690, 34.8888), // Tanzania
    ("UA", 48.3794, 31.1656), // Ukraine
    ("UG", 1.3733, 32.2903), // Uganda
    ("US", 37.0902, -95.7129), // United States
    ("UY", -32.5228, -55.7658), // Uruguay
    ("UZ", 41.3775, 64.5853), // Uzbekistan
    ("VA", 41.9029, 12.4534), // Vatican City
    ("VC", 12.9843, -61.2872), // Saint Vincent and the Grenadines
    ("VE", 6.4238, -66.5897), // Venezuela
    ("VG", 18.4207, -64.6400), // British Virgin Islands
    ("VI", 18.3358, -64.8963), // U.S. Virgin Islands
    ("VN", 14.0583, 108.2772), // Vietnam
    ("VU", -15.3767, 166.9592), // Vanuatu
    ("WF", -13.7688, -177.1561), // Wallis and Futuna
    ("WS", -13.7590, -172.1046), // Samoa
    ("XK", 42.6026, 20.9030), // Kosovo
    ("YE", 15.5527, 48.5164), // Yemen
    ("YT", -12.8275, 45.1662), // Mayotte
    ("ZA", -30.5595, 22.9375), // South Africa
    ("ZM", -13.1339, 27.8493), // Zambia
    ("ZW", -19.0154, 29.1549), // Zimbabwe
];

/// Centroid of a country code (case-insensitive), `None` for unknown codes.
pub fn centroid(code: &str) -> Option<(f64, f64)> {
    let code = code.to_ascii_uppercase();
    let i = CENTROIDS.binary_search_by(|(c, _, _)| c.cmp(&code.as_str())).ok()?;
    Some((CENTROIDS[i].1, CENTROIDS[i].2))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_unique_and_in_range() {
        assert!(CENTROIDS.len() > 200);
        for w in CENTROIDS.windows(2) {
            assert!(w[0].0 < w[1].0, "{} before {}: keep CENTROIDS sorted by code", w[0].0, w[1].0);
        }
        for (c, lat, lon) in CENTROIDS {
            assert_eq!(c.len(), 2, "{c}");
            assert!(c.bytes().all(|b| b.is_ascii_uppercase()), "{c}");
            assert!((-90.0..=90.0).contains(lat) && (-180.0..=180.0).contains(lon), "{c} {lat} {lon}");
        }
    }

    #[test]
    fn lookup() {
        let (lat, lon) = centroid("HU").unwrap();
        assert!((lat - 47.2).abs() < 0.5 && (lon - 19.5).abs() < 0.5, "{lat} {lon}");
        assert_eq!(centroid("us"), centroid("US"));
        assert!(centroid("US").unwrap().1 < -90.0);
        assert_eq!(centroid(""), None);
        assert_eq!(centroid("XX"), None);
        assert_eq!(centroid("USA"), None);
    }
}
