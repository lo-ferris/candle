use candle::{safetensors::Load, DType, Device, Error, Result, Shape, Tensor, Var};
use safetensors::{slice::IndexOp, tensor::SafeTensors};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// A `VarMap` is a store that holds named variables. Variables can be retrieved from the stores
/// and new variables can be added by providing some initialization config in case they are
/// missing.
/// `VarMap` structures can be serialized in the safetensors format.
#[derive(Clone)]
pub struct VarMap {
    data: Arc<Mutex<HashMap<String, Var>>>,
}

impl VarMap {
    /// Create a new empty `VarMap`.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let data = Arc::new(Mutex::new(HashMap::new()));
        Self { data }
    }

    /// Retrieve all the variables currently stored in the map.
    pub fn all_vars(&self) -> Vec<Var> {
        let tensor_data = self.data.lock().unwrap();
        #[allow(clippy::map_clone)]
        tensor_data.values().map(|c| c.clone()).collect::<Vec<_>>()
    }

    /// Save the map in the safetensors format.
    pub fn save<P: AsRef<std::path::Path>>(&self, path: P) -> Result<()> {
        let tensor_data = self.data.lock().unwrap();
        let data = tensor_data.iter().map(|(k, v)| (k, v.as_tensor()));
        safetensors::tensor::serialize_to_file(data, &None, path.as_ref())?;
        Ok(())
    }

    /// Load some values from a safetensors file and modify the existing variables to have these
    /// values.
    ///
    /// Note that values for variables that are currently not in the map are not kept.
    pub fn load<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref();
        let data = unsafe { candle::safetensors::MmapedFile::new(path)? };
        let data = data.deserialize()?;
        let mut tensor_data = self.data.lock().unwrap();
        for (name, var) in tensor_data.iter_mut() {
            match data.tensor(name) {
                Ok(data) => {
                    let data: Tensor = data.load(var.device())?;
                    if let Err(err) = var.set(&data) {
                        candle::bail!("error setting {name} using data from {path:?}: {err}",)
                    }
                }
                Err(_) => candle::bail!("cannot find tensor for {name}"),
            }
        }
        Ok(())
    }

    /// Retrieve or add a new variable.
    pub fn get<S: Into<Shape>>(
        &self,
        shape: S,
        path: &str,
        init: crate::Init,
        dtype: DType,
        device: &Device,
    ) -> Result<Tensor> {
        let shape = shape.into();
        let mut tensor_data = self.data.lock().unwrap();
        if let Some(tensor) = tensor_data.get(path) {
            let tensor_shape = tensor.shape();
            if &shape != tensor_shape {
                candle::bail!("shape mismatch on {path}: {shape:?} <> {tensor_shape:?}")
            }
            return Ok(tensor.as_tensor().clone());
        }
        let var = init.var(shape, dtype, device)?;
        let tensor = var.as_tensor().clone();
        tensor_data.insert(path.to_string(), var);
        Ok(tensor)
    }
}

// TODO: Maybe we would want the storage to be generic, e.g. with Box<dyn> to avoid too many
// generics.
enum Tensors<'a> {
    SafeTensorWithRouting {
        routing: HashMap<String, usize>,
        safetensors: Vec<SafeTensors<'a>>,
    },
    Npz(candle::npy::NpzTensors),
    TensorMap(HashMap<String, Tensor>),
    Zeros,
    VarMap(VarMap),
}

struct TensorData<'a> {
    tensors: Tensors<'a>,
    pub dtype: DType,
    pub device: Device,
}

impl<'a> TensorData<'a> {
    fn from_safetensors(safetensors: Vec<SafeTensors<'a>>, dtype: DType, device: &Device) -> Self {
        let mut routing = HashMap::new();
        for (index, sf) in safetensors.iter().enumerate() {
            for k in sf.names() {
                routing.insert(k.to_string(), index);
            }
        }
        let tensors = Tensors::SafeTensorWithRouting {
            routing,
            safetensors,
        };
        Self {
            tensors,
            device: device.clone(),
            dtype,
        }
    }

    fn zeros(dtype: DType, device: &Device) -> Self {
        Self {
            tensors: Tensors::Zeros,
            device: device.clone(),
            dtype,
        }
    }

    fn from_tensors(tensors: HashMap<String, Tensor>, dtype: DType, device: &Device) -> Self {
        Self {
            tensors: Tensors::TensorMap(tensors),
            device: device.clone(),
            dtype,
        }
    }

    fn from_npz<P: AsRef<std::path::Path>>(file: P, dtype: DType, device: &Device) -> Result<Self> {
        let npz = candle::npy::NpzTensors::new(file)?;
        Ok(Self {
            tensors: Tensors::Npz(npz),
            device: device.clone(),
            dtype,
        })
    }

    fn from_varmap(varmap: &VarMap, dtype: DType, device: &Device) -> Self {
        Self {
            tensors: Tensors::VarMap(varmap.clone()),
            device: device.clone(),
            dtype,
        }
    }
}

#[derive(Clone)]
pub struct VarBuilder<'a> {
    data: Arc<TensorData<'a>>,
    path: Vec<String>,
}

impl<'a> VarBuilder<'a> {
    /// Create a `VarBuilder` accessing data frome the safetensors storage. The initial path is
    /// set to the root path and sub-paths can be created via the `push_prefix` method.
    pub fn from_safetensors(st: Vec<SafeTensors<'a>>, dtype: DType, device: &Device) -> Self {
        let data = TensorData::from_safetensors(st, dtype, device);
        Self {
            data: Arc::new(data),
            path: vec![],
        }
    }

    pub fn zeros(dtype: DType, device: &Device) -> Self {
        let data = TensorData::zeros(dtype, device);
        Self {
            data: Arc::new(data),
            path: vec![],
        }
    }

    pub fn from_tensors(ts: HashMap<String, Tensor>, dtype: DType, device: &Device) -> Self {
        let data = TensorData::from_tensors(ts, dtype, device);
        Self {
            data: Arc::new(data),
            path: vec![],
        }
    }

    pub fn from_varmap(varmap: &VarMap, dtype: DType, device: &Device) -> Self {
        let data = TensorData::from_varmap(varmap, dtype, device);
        Self {
            data: Arc::new(data),
            path: vec![],
        }
    }

    pub fn from_npz<P: AsRef<std::path::Path>>(
        file: P,
        dtype: DType,
        device: &Device,
    ) -> Result<Self> {
        let data = TensorData::from_npz(file, dtype, device)?;
        Ok(Self {
            data: Arc::new(data),
            path: vec![],
        })
    }

    pub fn push_prefix(&self, s: &str) -> Self {
        let mut path = self.path.clone();
        path.push(s.to_string());
        Self {
            data: self.data.clone(),
            path,
        }
    }

    /// Short alias for `push_prefix`.
    pub fn pp(&self, s: &str) -> Self {
        self.push_prefix(s)
    }

    pub fn device(&self) -> &Device {
        &self.data.device
    }

    pub fn dtype(&self) -> DType {
        self.data.dtype
    }
}

impl<'a> VarBuilder<'a> {
    /// Get part of a tensor, typically used to do Tensor Parallelism sharding.
    ///
    /// If the tensor is of size (1024, 1024).
    ///
    /// `dim` corresponds to the dimension to slice into
    /// `rank` is the rank of the current process
    /// `world_size` is the total number of ranks in the process group
    ///
    /// `get_sharded("tensor", 0, 0, 2)` means `tensor.i((..512))`
    /// `get_sharded("tensor", 0, 1, 2)` means `tensor.i((512..))`
    /// `get_sharded("tensor", 1, 0, 2)` means `tensor.i((.., ..512))`
    pub fn get_sharded(
        &self,
        tensor_name: &str,
        dim: usize,
        rank: usize,
        world_size: usize,
    ) -> Result<Tensor> {
        let data = self.data.as_ref();
        let path = self.path(tensor_name);
        let tensor = match &self.data.tensors {
            Tensors::SafeTensorWithRouting {
                routing,
                safetensors,
            } => {
                let index = routing.get(&path).ok_or_else(|| {
                    Error::CannotFindTensor {
                        path: path.to_string(),
                    }
                    .bt()
                })?;

                let view = safetensors[*index].tensor(&path)?;
                let dtype = view.dtype();
                let mut shape = view.shape().to_vec();
                let size = shape[dim];

                if size % world_size != 0 {
                    return Err(Error::ShapeMismatchSplit {
                        shape: shape.into(),
                        dim,
                        n_parts: world_size,
                    });
                }
                let block_size = size / world_size;
                let start = rank * block_size;
                let stop = (rank + 1) * block_size;

                // Everything is expressed in tensor dimension
                // bytes offsets is handled automatically for safetensors.

                let iterator = if dim == 0 {
                    view.slice(start..stop).map_err(|_| Error::Msg(format!("Cannot slice tensor {tensor_name} ({shape:?} along dim {dim} with {start}..{stop}")))?
                } else if dim == 1 {
                    view.slice((.., start..stop)).map_err(|_| Error::Msg(format!("Cannot slice tensor {tensor_name} ({shape:?} along dim {dim} with {start}..{stop}")))?
                } else {
                    candle::bail!("Get sharded on dimensions != 0 or 1")
                };

                shape[dim] = block_size;

                let dtype: DType = dtype.try_into()?;

                let raw: Vec<u8> = iterator.into_iter().flatten().cloned().collect();
                Tensor::from_raw_buffer(&raw, dtype, &shape, &data.device)?
            }
            _ => candle::bail!("get_sharded is only available for safetensors"),
        };
        Ok(tensor)
    }

    /// Retrieve the tensor associted with the current name and path.
    pub fn get<S: Into<Shape>>(&self, s: S, tensor_name: &str) -> Result<Tensor> {
        let data = self.data.as_ref();
        let s: Shape = s.into();
        let path = self.path(tensor_name);
        let tensor = match &self.data.tensors {
            Tensors::Zeros => Tensor::zeros(&s, data.dtype, &data.device)?.contiguous()?,
            Tensors::TensorMap(ts) => ts
                .get(&path)
                .ok_or_else(|| {
                    Error::CannotFindTensor {
                        path: path.to_string(),
                    }
                    .bt()
                })?
                .clone(),
            Tensors::VarMap(varmap) => {
                let data = varmap.data.lock().unwrap();
                data.get(&path)
                    .ok_or_else(|| {
                        Error::CannotFindTensor {
                            path: path.to_string(),
                        }
                        .bt()
                    })?
                    .as_tensor()
                    .clone()
            }
            Tensors::Npz(npz) => npz.get(&path)?.ok_or_else(|| {
                Error::CannotFindTensor {
                    path: path.to_string(),
                }
                .bt()
            })?,
            Tensors::SafeTensorWithRouting {
                routing,
                safetensors,
            } => {
                let index = routing.get(&path).ok_or_else(|| {
                    Error::CannotFindTensor {
                        path: path.to_string(),
                    }
                    .bt()
                })?;
                safetensors[*index]
                    .tensor(&path)?
                    .load(&data.device)?
                    .to_dtype(data.dtype)?
            }
        };
        if tensor.shape() != &s {
            Err(candle::Error::UnexpectedShape {
                msg: format!("shape mismatch for {path}"),
                expected: s,
                got: tensor.shape().clone(),
            }
            .bt())?
        }
        Ok(tensor)
    }

    /// Retrieve the tensor associted with the current name and path or initialize a new tensor if
    /// it's missing.
    ///
    /// Tensor initialization is only available if the `VarBuilder` is backed by a `VarMap`.
    pub fn get_or_init<S: Into<Shape>>(
        &self,
        s: S,
        tensor_name: &str,
        init: crate::Init,
    ) -> Result<Tensor> {
        let data = self.data.as_ref();
        match &self.data.tensors {
            Tensors::VarMap(varmap) => {
                let path = self.path(tensor_name);
                varmap.get(s, &path, init, data.dtype, &data.device)
            }
            _ => self.get(s, tensor_name),
        }
    }

    fn path(&self, tensor_name: &str) -> String {
        if self.path.is_empty() {
            tensor_name.to_string()
        } else {
            [&self.path.join("."), tensor_name].join(".")
        }
    }
}
