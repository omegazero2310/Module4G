use super::*;

impl Store {
    pub fn replace_audio_inventory(&self, audio: &[UploadedAudioRecord]) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute("DELETE FROM uploaded_audio", [])
            .map_err(db_error)?;
        for item in audio {
            transaction.execute(
                "INSERT INTO uploaded_audio(id,name,format,size,module_path,duration_ms,created_at_ms,is_current) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![item.id,item.name,item.format,item.size,item.module_path,item.duration_ms,item.created_at_ms,item.is_current],
            ).map_err(db_error)?;
        }
        transaction.commit().map_err(db_error)
    }

    pub fn current_audio(&self) -> Result<Option<UploadedAudioRecord>, ModemError> {
        self.connection()?
            .query_row(
                "SELECT id,name,format,size,module_path,duration_ms,created_at_ms,is_current FROM uploaded_audio WHERE is_current=1 LIMIT 1",
                [],
                |row| {
                    Ok(UploadedAudioRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        format: row.get(2)?,
                        size: row.get(3)?,
                        module_path: row.get(4)?,
                        duration_ms: row.get(5)?,
                        created_at_ms: row.get(6)?,
                        state: "ready".into(),
                        is_current: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(db_error)
    }

    pub fn save_current_audio(&self, audio: &UploadedAudioRecord) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE uploaded_audio SET is_current=0 WHERE is_current=1",
                [],
            )
            .map_err(db_error)?;
        transaction.execute(
            "INSERT INTO uploaded_audio(id,name,format,size,module_path,duration_ms,created_at_ms,is_current) VALUES(?1,?2,?3,?4,?5,?6,?7,1) ON CONFLICT(id) DO UPDATE SET name=excluded.name,format=excluded.format,size=excluded.size,module_path=excluded.module_path,duration_ms=excluded.duration_ms,created_at_ms=excluded.created_at_ms,is_current=1",
            params![audio.id,audio.name,audio.format,audio.size,audio.module_path,audio.duration_ms,audio.created_at_ms],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn list_audio(&self) -> Result<Vec<UploadedAudioRecord>, ModemError> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT id,name,format,size,module_path,duration_ms,created_at_ms,is_current FROM uploaded_audio ORDER BY is_current DESC,created_at_ms DESC,id DESC",
        ).map_err(db_error)?;
        statement
            .query_map([], audio_from_row)
            .map_err(db_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(db_error)
    }

    pub fn audio_named(&self, name: &str) -> Result<Option<UploadedAudioRecord>, ModemError> {
        self.connection()?.query_row(
            "SELECT id,name,format,size,module_path,duration_ms,created_at_ms,is_current FROM uploaded_audio WHERE lower(trim(name))=lower(trim(?1)) LIMIT 1",
            [name], audio_from_row,
        ).optional().map_err(db_error)
    }

    pub fn replace_and_select_audio(
        &self,
        audio: &UploadedAudioRecord,
        replaced_id: Option<&str>,
    ) -> Result<(), ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        transaction
            .execute(
                "UPDATE uploaded_audio SET is_current=0 WHERE is_current=1",
                [],
            )
            .map_err(db_error)?;
        if let Some(id) = replaced_id {
            transaction
                .execute("DELETE FROM uploaded_audio WHERE id=?1", [id])
                .map_err(db_error)?;
        }
        transaction.execute(
            "INSERT INTO uploaded_audio(id,name,format,size,module_path,duration_ms,created_at_ms,is_current) VALUES(?1,?2,?3,?4,?5,?6,?7,1)",
            params![audio.id,audio.name,audio.format,audio.size,audio.module_path,audio.duration_ms,audio.created_at_ms],
        ).map_err(db_error)?;
        transaction.commit().map_err(db_error)
    }

    pub fn select_audio(&self, id: &str) -> Result<UploadedAudioRecord, ModemError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction().map_err(db_error)?;
        let exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM uploaded_audio WHERE id=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(db_error)?;
        if !exists {
            return Err(ModemError::Validation("audio file was not found".into()));
        }
        transaction
            .execute(
                "UPDATE uploaded_audio SET is_current=0 WHERE is_current=1",
                [],
            )
            .map_err(db_error)?;
        transaction
            .execute("UPDATE uploaded_audio SET is_current=1 WHERE id=?1", [id])
            .map_err(db_error)?;
        transaction.commit().map_err(db_error)?;
        drop(connection);
        self.current_audio()?
            .ok_or_else(|| ModemError::Validation("audio file was not found".into()))
    }
}
