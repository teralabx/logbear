import { redirect } from '@sveltejs/kit';
import type { PageLoad } from './$types';

// /project/:id has no content of its own — Live is where you land.
export const load: PageLoad = ({ params }) => {
	redirect(307, `/project/${params.id}/live`);
};
