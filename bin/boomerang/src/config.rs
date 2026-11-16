use yamlite::FromYaml;

#[derive(Debug, PartialEq, FromYaml)]
pub(crate) struct BoomerangConfig<'a> {
    pub(crate) database: Database<'a>,
}

#[derive(Debug, PartialEq, FromYaml)]
pub(crate) struct Database<'a> {
    pub(crate) url: &'a str,
}
