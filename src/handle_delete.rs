
use futures::prelude::*;
use futures03::compat::Future01CompatExt;

use http::{Request, Response, StatusCode};

use crate::{BoxedByteStream,DavResult};
use crate::conditional::if_match_get_tokens;
use crate::errors::*;
use crate::fs::*;
use crate::headers::Depth;
use crate::makestream;
use crate::multierror::{MultiError, multi_error};
use crate::typed_headers::HeaderMapExt;
use crate::webpath::WebPath;

// map_err helper.
async fn add_status<'a>(m_err: &'a mut MultiError, path: &'a WebPath, e: FsError) -> DavError {
    let status = DavError::FsError(e).statuscode();
    if let Err(x) = await!(m_err.add_status(path, status)) {
        return x.into();
    }
    DavError::Status(status)
}

// map_err helper for directories, the result statuscode
// mappings are not 100% the same.
async fn dir_status<'a>(res: &'a mut MultiError, path: &'a WebPath, e: FsError) -> DavError {
    let status = match e {
        FsError::Exists => StatusCode::CONFLICT,
        e => DavError::FsError(e).statuscode(),
    };
    if let Err(x) = await!(res.add_status(path, status)) {
        return x.into();
    }
    DavError::Status(status)
}

impl crate::DavInner {

    pub(crate) async fn delete_items<'a>(&'a self, mut res: &'a mut MultiError, depth: Depth, meta: Box<DavMetaData + 'a>, path: &'a WebPath) -> DavResult<()> {
        if !meta.is_dir() {
            debug!("delete_items (file) {} {:?}", path, depth);
            return match blocking_io!(self.fs.remove_file(path)) {
                Ok(x) => Ok(x),
                Err(e) => Err(await!(add_status(&mut res, path, e))),
            };
        }
        if depth == Depth::Zero {
            debug!("delete_items (dir) {} {:?}", path, depth);
            return match blocking_io!(self.fs.remove_dir(path)) {
                Ok(x) => Ok(x),
                Err(e) => Err(await!(add_status(&mut res, path, e))),
            };
        }
        debug!("delete_items (recurse) {} {:?}", path, depth);

        // walk over all entries.
        let mut entries = match blocking_io!(self.fs.read_dir(path)) {
            Ok(x) => Ok(x),
            Err(e) => Err(await!(add_status(&mut res, path, e))),
        }?;
        let mut result = Ok(());
        // XXX FIXME IMPORTANT WRAP IT IN BLOCKING_IO!
        while let Some(dirent) = entries.next() {
            // if metadata() fails, skip to next entry.
            // NOTE: dirent.metadata == symlink_metadata (!)
            let meta = match blocking_io!(dirent.metadata()) {
                Ok(m) => m,
                Err(e) => {
                    result = Err(await!(add_status(&mut res, path, e)));
                    continue
                },
            };

            let mut npath = path.clone();
            npath.push_segment(&dirent.name());
            npath.add_slash_if(meta.is_dir());

            // do the actual work. If this fails with a non-fs related error,
            // return immediately.
            // XXX FIXME EVEN MORE IMPORTANT UNCOMMENT AND FIX THIS XXX
            panic!("won't work");
            /*
            if let Err(e) = await!(self.delete_items(&mut res, depth, meta, &npath)) {
                match e {
                    DavError::Status(_) => {
                        result = Err(e);
                          continue;
                    },
                    _ => return Err(e),
                }
            }
            */
        }

        // if we got any error, return with the error,
        // and do not try to remove the directory.
        result?;

        match blocking_io!(self.fs.remove_dir(path)) {
            Ok(x) => Ok(x),
            Err(e) => Err(await!(dir_status(&mut res, path, e))),
        }
    }

    pub(crate) async fn handle_delete(self, req: Request<()>)
        -> Result<Response<BoxedByteStream>, DavError>
    {
        // RFC4918 9.6.1 DELETE for Collections.
        // Note that allowing Depth: 0 is NOT RFC compliant.
        let depth = match req.headers().typed_get::<Depth>() {
            Some(Depth::Infinity) | None => Depth::Infinity,
            Some(Depth::Zero) => Depth::Zero,
            _ => return Err(DavError::Status(StatusCode::BAD_REQUEST)),
        };

        let mut path = self.path(&req);
        let meta = blocking_io!(self.fs.symlink_metadata(&path))?;
        if meta.is_symlink() {
            if let Ok(m2) = blocking_io!(self.fs.metadata(&path)) {
                path.add_slash_if(m2.is_dir());
            }
        }
        path.add_slash_if(meta.is_dir());

        // check the If and If-* headers.
        let tokens_res = await!(if_match_get_tokens(&req, Some(&meta), &self.fs, &self.ls, &path));
        let tokens = match tokens_res {
            Ok(t) => t,
            Err(s) => return Err(DavError::Status(s)),
        };

        // check locks. since we cancel the entire operation if there is
        // a conflicting lock, we do not return a 207 multistatus, but
        // just a simple status.
        if let Some(ref locksystem) = self.ls {
            let t = tokens.iter().map(|s| s.as_str()).collect::<Vec<&str>>();
            let principal = self.principal.as_ref().map(|s| s.as_str());
            if let Err(_l) = locksystem.check(&path, principal, false, true, t) {
                return Err(DavError::Status(StatusCode::LOCKED));
            }
        }

        let path2 = path.clone();
        let items = makestream::stream03(async move |tx| {

            // turn the Sink into something easier to pass around.
            let mut multierror = MultiError::new(tx);

            // now delete the path recursively.
            if let Ok(()) = await!(self.delete_items(&mut multierror, depth, meta, &path)) {
                // Done. Now delete the path in the locksystem as well.
                // Should really do this per resource, in case the delete partially fails. See TODO.pm
                if let Some(ref locksystem) = self.ls {
                    locksystem.delete(&path).ok();
                }
            }
            Ok(())
        });

        await!(multi_error(path2, items))
    }
}

