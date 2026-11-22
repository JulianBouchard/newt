import { readFile } from "eve/tools/read_file";
import { afterPages } from "../../lib/mock";

export default afterPages(readFile);
