use std::collections::HashMap;
use std::io::SeekFrom;
use std::marker::PhantomData;
use std::mem;
use std::pin::Pin;

use super::reader::{
    MzMLParserError, MzMLParserState, SpectrumBuilding,
    Bytes, FileMetadataBuilder, MzMLSpectrumBuilder,
    IndexParserState, XMLParseBase, MzMLIndexingError,
    IncrementingIdMap
};

use tokio::{self, io};
use tokio::io::{AsyncSeek, AsyncRead, BufReader, AsyncSeekExt, AsyncReadExt};

use log::{debug, warn};

use quick_xml::events::{Event, BytesStart, BytesEnd, BytesText};
use quick_xml::Error as XMLError;
use quick_xml::Reader;


use crate::SpectrumBehavior;

use crate::meta::file_description::FileDescription;
use crate::meta::instrument::InstrumentConfiguration;
use crate::meta::{DataProcessing, MSDataFileMetadata, Software};
use crate::params::Param;
use crate::spectrum::spectrum::{
    CentroidPeakAdapting, DeconvolutedPeakAdapting, MultiLayerSpectrum,
};

use super::super::offset_index::OffsetIndex;
// Need to learn more about async traits
// use super::super::traits::{
//     MZFileReader, RandomAccessSpectrumIterator, ScanAccessError, ScanSource,
// };

pub trait AsyncReadType : AsyncRead + AsyncReadExt {}

impl<T> AsyncReadType for T where T : AsyncRead + AsyncReadExt {}

use mzpeaks::{CentroidPeak, DeconvolutedPeak};

const BUFFER_SIZE: usize = 10000;

pub struct MzMLReaderType<
    R: AsyncReadType + Unpin,
    C: CentroidPeakAdapting + Send + Sync = CentroidPeak,
    D: DeconvolutedPeakAdapting + Send + Sync = DeconvolutedPeak,
> {
    /// The state the parser was in last.
    pub state: MzMLParserState,
    /// The raw reader
    pub handle: BufReader<R>,
    /// A place to store the last error the parser encountered
    error: MzMLParserError,
    /// A spectrum ID to byte offset for fast random access
    pub index: OffsetIndex,
    /// The description of the file's contents and the previous data files that were
    /// consumed to produce it.
    pub file_description: FileDescription,
    /// A mapping of different instrument configurations (source, analyzer, detector) components
    /// by ID string.
    pub instrument_configurations: HashMap<u32, InstrumentConfiguration>,
    /// The different software components that were involved in the processing and creation of this
    /// file.
    pub softwares: Vec<Software>,
    /// The data processing and signal transformation operations performed on the raw data in previous
    /// source files to produce this file's contents.
    pub data_processings: Vec<DataProcessing>,
    /// A cache of repeated paramters
    pub reference_param_groups: HashMap<String, Vec<Param>>,

    pub(crate) instrument_id_map: IncrementingIdMap,
    buffer: Bytes,
    centroid_type: PhantomData<C>,
    deconvoluted_type: PhantomData<D>,

}

impl<R: AsyncReadType + Unpin + Sync, C: CentroidPeakAdapting + Send  + Sync, D: DeconvolutedPeakAdapting + Send + Sync> MzMLReaderType<R, C, D> {
    /// Create a new [`MzMLReaderType`] instance, wrapping the [`io::Read`] handle
    /// provided with an `[io::BufReader`] and parses the metadata section of the file.
    pub async fn new(file: R) -> MzMLReaderType<R, C, D> {
        let handle = BufReader::with_capacity(BUFFER_SIZE, file);
        let mut inst = MzMLReaderType {
            handle,
            state: MzMLParserState::Start,
            error: MzMLParserError::default(),
            buffer: Bytes::new(),
            index: OffsetIndex::new("spectrum".to_owned()),

            file_description: FileDescription::default(),
            instrument_configurations: HashMap::new(),
            softwares: Vec::new(),
            data_processings: Vec::new(),
            reference_param_groups: HashMap::new(),

            centroid_type: PhantomData,
            deconvoluted_type: PhantomData,
            instrument_id_map: IncrementingIdMap::default()
        };
        match inst.parse_metadata().await {
            Ok(()) => {}
            Err(_err) => {}
        }
        inst
    }

    /**Parse the metadata section of the file using [`FileMetadataBuilder`]
     */
    async fn parse_metadata(&mut self) -> Result<(), MzMLParserError> {
        let mut reader = Reader::from_reader(&mut self.handle);
        reader.trim_text(true);
        let mut accumulator = FileMetadataBuilder::default();
        accumulator.instrument_id_map.copy_from(&self.instrument_id_map);
        loop {
            match reader.read_event_into_async(&mut self.buffer).await {
                Ok(Event::Start(ref e)) => {
                    match accumulator.start_element(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                            match &self.state {
                                MzMLParserState::Run
                                | MzMLParserState::SpectrumList
                                | MzMLParserState::Spectrum => break,
                                _ => {}
                            }
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::End(ref e)) => {
                    match accumulator.end_element(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::Text(ref e)) => {
                    match accumulator.text(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::Empty(ref e)) => {
                    match accumulator.empty_element(e, self.state, reader.buffer_position()) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    }
                }
                Ok(Event::Eof) => {
                    break;
                }
                Err(err) => match &err {
                    XMLError::EndEventMismatch {
                        expected,
                        found: _found,
                    } => {
                        if expected.is_empty() && self.state == MzMLParserState::Resume {
                            continue;
                        } else {
                            self.error = MzMLParserError::IncompleteElementError(
                                String::from_utf8_lossy(&self.buffer).to_owned().to_string(),
                                self.state,
                            );
                            self.state = MzMLParserState::ParserError;
                        }
                    }
                    _ => {
                        self.error = MzMLParserError::IncompleteElementError(
                            String::from_utf8_lossy(&self.buffer).to_owned().to_string(),
                            self.state,
                        );
                        self.state = MzMLParserState::ParserError;
                    }
                },
                _ => {}
            };
            self.buffer.clear();
            match self.state {
                MzMLParserState::Run | MzMLParserState::ParserError => {
                    break;
                }
                _ => {}
            };
        }
        self.file_description = accumulator.file_description;
        self.instrument_configurations = accumulator
            .instrument_configurations
            .into_iter()
            .map(|ic| (ic.id.clone(), ic))
            .collect();
        self.softwares = accumulator.softwares;
        self.data_processings = accumulator.data_processings;
        self.reference_param_groups = accumulator.reference_param_groups;
        self.instrument_id_map.copy_from(&accumulator.instrument_id_map);
        match self.state {
            MzMLParserState::SpectrumDone => Ok(()),
            MzMLParserState::ParserError => {
                let mut error = MzMLParserError::NoError;
                mem::swap(&mut error, &mut self.error);
                Err(error)
            }
            _ => Err(MzMLParserError::IncompleteSpectrum),
        }
    }


    async fn _parse_into(
        &mut self,
        accumulator: &mut MzMLSpectrumBuilder<C, D>,
    ) -> Result<usize, MzMLParserError> {
        let mut reader = Reader::from_reader(&mut self.handle);
        reader.trim_text(true);
        accumulator.instrument_id_map.copy_from(&self.instrument_id_map);
        let mut offset: usize = 0;
        loop {
            let event = reader.read_event_into_async(&mut self.buffer).await;
            match event {
                Ok(Event::Start(ref e)) => {
                    match accumulator.start_element(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::End(ref e)) => {
                    match accumulator.end_element(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::Text(ref e)) => {
                    match accumulator.text(e, self.state) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    };
                }
                Ok(Event::Empty(ref e)) => {
                    match accumulator.empty_element(e, self.state, reader.buffer_position()) {
                        Ok(state) => {
                            self.state = state;
                        }
                        Err(message) => {
                            self.state = MzMLParserState::ParserError;
                            self.error = message;
                        }
                    }
                }
                Ok(Event::Eof) => {
                    break;
                }
                Err(err) => match &err {
                    XMLError::EndEventMismatch {
                        expected,
                        found: _found,
                    } => {
                        if expected.is_empty() && self.state == MzMLParserState::Resume {
                            continue;
                        } else {
                            self.error = MzMLParserError::IncompleteElementError(
                                String::from_utf8_lossy(&self.buffer).to_owned().to_string(),
                                self.state,
                            );
                            self.state = MzMLParserState::ParserError;
                        }
                    }
                    _ => {
                        self.error = MzMLParserError::IncompleteElementError(
                            String::from_utf8_lossy(&self.buffer).to_owned().to_string(),
                            self.state,
                        );
                        self.state = MzMLParserState::ParserError;
                    }
                },
                _ => {}
            };
            offset += self.buffer.len();
            self.buffer.clear();
            self.instrument_id_map.copy_from(&accumulator.instrument_id_map);
            match self.state {
                MzMLParserState::SpectrumDone | MzMLParserState::ParserError => {
                    break;
                }
                _ => {}
            };
        }
        match self.state {
            MzMLParserState::SpectrumDone => Ok(offset),
            MzMLParserState::ParserError => {
                let mut error = MzMLParserError::NoError;
                mem::swap(&mut error, &mut self.error);
                Err(error)
            }
            _ => Err(MzMLParserError::IncompleteSpectrum),
        }
    }

    /// Populate a new [`Spectrum`] in-place on the next available spectrum data.
    /// This allocates memory to build the spectrum's attributes but then moves it
    /// into `spectrum` rather than copying it.
    pub async fn read_into(
        &mut self,
        spectrum: &mut MultiLayerSpectrum<C, D>,
    ) -> Result<usize, MzMLParserError> {
        let mut accumulator = MzMLSpectrumBuilder::<C, D>::new();
        if self.state == MzMLParserState::SpectrumDone {
            self.state = MzMLParserState::Resume;
        }
        match self._parse_into(&mut accumulator).await {
            Ok(sz) => {
                accumulator.into_spectrum(spectrum);
                Ok(sz)
            }
            Err(err) => Err(err),
        }
    }

    /// Read the next spectrum directly. Used to implement iteration.
    pub async fn read_next(&mut self) -> Option<MultiLayerSpectrum<C, D>> {
        let mut spectrum = MultiLayerSpectrum::<C, D>::default();
        match self.read_into(&mut spectrum).await {
            Ok(_sz) => {
                Some(spectrum)
            },
            Err(err) => {
                debug!("Failed to read next spectrum: {err}");
                None
            },
        }
    }
}

impl<R: AsyncReadType + Unpin, C: CentroidPeakAdapting + Send + Sync, D: DeconvolutedPeakAdapting + Send + Sync> MSDataFileMetadata
    for MzMLReaderType<R, C, D>
{
    crate::impl_metadata_trait!();
}

/// A specialization of [`MzMLReaderType`] for the default peak types, for common use.
pub type MzMLReader<R> = MzMLReaderType<R, CentroidPeak, DeconvolutedPeak>;



#[derive(Debug, Default, Clone)]
pub struct IndexedMzMLIndexExtractor {
    spectrum_index: OffsetIndex,
    chromatogram_index: OffsetIndex,
    last_id: String,
}

impl XMLParseBase for IndexedMzMLIndexExtractor {}

impl IndexedMzMLIndexExtractor {
    pub fn new() -> IndexedMzMLIndexExtractor {
        IndexedMzMLIndexExtractor {
            spectrum_index: OffsetIndex::new("spectrum".into()),
            chromatogram_index: OffsetIndex::new("chromatogram".into()),
            last_id: String::new(),
        }
    }

    pub async fn find_offset_from_reader<R: AsyncReadType + AsyncSeek + AsyncSeekExt + Unpin>(&self, reader: &mut Pin<&mut R>) -> io::Result<Option<u64>> {
        reader.seek(SeekFrom::End(-200)).await?;
        let mut buf = Bytes::new();
        reader.read_to_end(&mut buf).await?;
        let pattern = regex::Regex::new("<indexListOffset>(\\d+)</indexListOffset>").unwrap();
        if let Some(captures) = pattern.captures(&String::from_utf8_lossy(&buf)) {
            if let Some(offset) = captures.get(1) {
                if let Ok(offset) = offset.as_str().parse::<u64>() {
                    return Ok(Some(offset));
                }
            }
        }
        Ok(None)
    }

    pub fn start_element(
        &mut self,
        event: &BytesStart,
        state: IndexParserState,
    ) -> Result<IndexParserState, XMLError> {
        let elt_name = event.name();
        match elt_name.as_ref() {
            b"offset" => {
                for attr_parsed in event.attributes() {
                    match attr_parsed {
                        Ok(attr) => {
                            if attr.key.as_ref() == b"idRef" {
                                self.last_id = attr
                                    .unescape_value()
                                    .expect("Error decoding idRef").to_string();
                            }
                        }
                        Err(err) => {
                            return Err(err.into());
                        }
                    }
                }
            }
            b"index" => {
                for attr_parsed in event.attributes() {
                    match attr_parsed {
                        Ok(attr) => {
                            if attr.key.as_ref() == b"name" {
                                let index_name = attr
                                    .unescape_value()
                                    .expect("Error decoding idRef").to_string();
                                match index_name.as_ref() {
                                    "spectrum" => return Ok(IndexParserState::SpectrumIndexList),
                                    "chromatogram" => {
                                        return Ok(IndexParserState::ChromatogramIndexList)
                                    }
                                    _ => {}
                                }
                            }
                        }
                        Err(err) => {
                            return Err(err.into());
                        }
                    }
                }
            }
            b"indexList" => {}
            _ => {}
        }

        Ok(state)
    }

    pub fn end_element(
        &mut self,
        event: &BytesEnd,
        state: IndexParserState,
    ) -> Result<IndexParserState, XMLError> {
        let elt_name = event.name();
        match elt_name.as_ref() {
            b"offset" => {}
            b"index" => {}
            b"indexList" => return Ok(IndexParserState::Done),
            _ => {}
        }
        Ok(state)
    }

    pub fn text(
        &mut self,
        event: &BytesText,
        state: IndexParserState,
    ) -> Result<IndexParserState, XMLError> {
        match state {
            IndexParserState::SpectrumIndexList => {
                let bin = event
                    .unescape()
                    .expect("Failed to unescape spectrum offset");
                if let Ok(offset) = bin.parse::<u64>() {
                    if self.last_id != "" {
                        let key = mem::take(&mut self.last_id);
                        self.spectrum_index.insert(key, offset);
                    } else {
                        warn!("Out of order text in index")
                    }
                }
            }
            IndexParserState::ChromatogramIndexList => {
                let bin = event
                    .unescape()
                    .expect("Failed to unescape chromatogram offset");
                if let Ok(offset) = bin.parse::<u64>() {
                    if self.last_id != "" {
                        let key = mem::take(&mut self.last_id);
                        self.chromatogram_index.insert(key, offset);
                    } else {
                        warn!("Out of order text in index")
                    }
                }
            }
            _ => {}
        }
        Ok(state)
    }
}


impl<R: AsyncReadType + AsyncSeek + AsyncSeekExt + Unpin + Sync, C: CentroidPeakAdapting  + Send + Sync, D: DeconvolutedPeakAdapting  + Send + Sync> MzMLReaderType<R, C, D> {
    pub async fn read_index_from_end(&mut self) -> Result<u64, MzMLIndexingError> {
        let mut indexer = IndexedMzMLIndexExtractor::new();
        let current_position = match self.handle.stream_position().await {
            Ok(position) => position,
            Err(err) => return Err(MzMLIndexingError::IOError(err)),
        };
        let mut handle = Pin::new(&mut self.handle);
        let offset = match indexer.find_offset_from_reader(&mut handle).await {
            Ok(offset) => {
                if let Some(offset) = offset {
                    offset
                } else {
                    return Err(MzMLIndexingError::OffsetNotFound);
                }
            }
            Err(err) => return Err(MzMLIndexingError::IOError(err)),
        };
        let mut indexer_state = IndexParserState::Start;
        self.handle.seek(SeekFrom::Start(offset)).await.expect("Failed to seek to the index offset");

        let mut reader = Reader::from_reader(&mut self.handle);
        reader.trim_text(true);

        loop {
            match reader.read_event_into_async(&mut self.buffer).await {
                Ok(Event::Start(ref e)) => {
                    match indexer.start_element(e, indexer_state) {
                        Ok(state) => {
                            indexer_state = state;
                            match &indexer_state {
                                IndexParserState::Done => break,
                                _ => {}
                            }
                        }
                        Err(message) => return Err(MzMLIndexingError::XMLError(message)),
                    };
                }
                Ok(Event::End(ref e)) => {
                    match indexer.end_element(e, indexer_state) {
                        Ok(state) => {
                            indexer_state = state;
                        }
                        Err(message) => return Err(MzMLIndexingError::XMLError(message)),
                    };
                }
                Ok(Event::Text(ref e)) => {
                    match indexer.text(e, indexer_state) {
                        Ok(state) => {
                            indexer_state = state;
                        }
                        Err(message) => return Err(MzMLIndexingError::XMLError(message)),
                    };
                }
                Ok(Event::Eof) => {
                    break;
                }
                Err(err) => return Err(MzMLIndexingError::XMLError(err)),
                _ => {}
            }
        }
        self.buffer.clear();
        self.index = indexer.spectrum_index;
        self.index.init = true;
        self.handle.seek(SeekFrom::Start(current_position)).await.unwrap();
        Ok(self.index.len() as u64)
    }

    /// Helper method to support seeking to an ID
    fn _offset_of_id(&self, id: &str) -> Option<u64> {
        self.get_index().get(id)
    }

    /// Helper method to support seeking to an index
    fn _offset_of_index(&self, index: usize) -> Option<u64> {
        self.get_index()
            .get_index(index)
            .map(|(_id, offset)| offset)
    }

    /// Helper method to support seeking to a specific time.
    /// Considerably more complex than seeking by ID or index.
    async fn _offset_of_time(&mut self, time: f64) -> Option<u64> {
        match self.get_spectrum_by_time(time).await {
            Some(scan) => self._offset_of_index(scan.index()),
            None => None,
        }
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// Retrieve a spectrum by its scan start time
    /// Considerably more complex than seeking by ID or index.
    pub async fn get_spectrum_by_time(&mut self, time: f64) -> Option<MultiLayerSpectrum<C, D>> {
        let n = self.len();
        let mut lo: usize = 0;
        let mut hi: usize = n;

        let mut best_error: f64 = f64::INFINITY;
        let mut best_match: Option<MultiLayerSpectrum<_, _>> = None;

        if lo == hi {
            return None;
        }
        while hi != lo {
            let mid = (hi + lo) / 2;
            let scan = self.get_spectrum_by_index(mid).await?;
            let scan_time = scan.start_time();
            let err = (scan_time - time).abs();

            if err < best_error {
                best_error = err;
                best_match = Some(scan);
            } else if (scan_time - time).abs() < 1e-3 {
                return Some(scan);
            } else if scan_time > time {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        best_match
    }

    /// Retrieve a spectrum by it's native ID
    pub async fn get_spectrum_by_id(&mut self, id: &str) -> Option<MultiLayerSpectrum<C, D>> {
        let offset_ref = self.index.get(id);
        let offset = offset_ref.expect("Failed to retrieve offset");
        let start = self
            .handle
            .stream_position().await
            .expect("Failed to save checkpoint");
        self.handle.seek(SeekFrom::Start(offset)).await
            .expect("Failed to move seek to offset");
        let result = self.read_next().await;
        self.handle.seek(SeekFrom::Start(start)).await
            .expect("Failed to restore offset");
        result
    }

    /// Retrieve a spectrum by it's integer index
    pub async fn get_spectrum_by_index(&mut self, index: usize) -> Option<MultiLayerSpectrum<C, D>> {
        let (_id, offset) = self.index.get_index(index)?;
        let byte_offset = offset;
        let start = self
            .handle
            .stream_position().await
            .expect("Failed to save checkpoint");
        self.handle.seek(SeekFrom::Start(byte_offset)).await.ok()?;
        let result = self.read_next().await;
        self.handle.seek(SeekFrom::Start(start)).await
            .expect("Failed to restore offset");
        result
    }

    /// Return the data stream to the beginning
    pub async fn reset(&mut self) {
        self.handle.seek(SeekFrom::Start(0)).await
            .expect("Failed to reset file stream");
    }

    pub fn get_index(&self) -> &OffsetIndex {
        if !self.index.init {
            warn!("Attempting to use an uninitialized offset index on MzMLReaderType")
        }
        &self.index
    }

    pub fn set_index(&mut self, index: OffsetIndex) {
        self.index = index
    }
}



#[cfg(test)]
mod test {
    use std::path;

    use crate::{SpectrumBehavior, ParamDescribed};

    use super::*;
    use tokio::{fs, io};


    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_open() -> io::Result<()> {
        let path = path::Path::new("./test/data/read_index_of.mzML");
        let file = fs::File::open(path).await?;
        let mut reader = MzMLReader::new(file).await;

        let mut ms1_counter = 0;
        let mut msn_counter = 0;
        while let Some(spec) = reader.read_next().await {
            let filter_string = spec.acquisition().first_scan().unwrap().get_param_by_accession("MS:1000512").unwrap();
            let configs = spec.acquisition().instrument_configuration_ids();
            let conf = configs[0];
            println!("Processing scan {}", spec.index());
            dbg!(configs, &filter_string.value);
            if filter_string.value.contains("ITMS") {
                assert_eq!(conf, 1);
            } else {
                assert_eq!(conf, 0);
            }
            if spec.ms_level() > 1 {
                msn_counter += 1;
            } else {
                ms1_counter += 1;
            }

        }

        assert_eq!(ms1_counter, 14);
        assert_eq!(msn_counter, 34);

        Ok(())
    }
}