import { createListView } from "./list_view.js";
import { createHttpApi } from "./http_api.js";

const { load } = createListView(createHttpApi());
void load();
